use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use similar::TextDiff;
use uuid::Uuid;

use crate::agent_policy::{FilesystemPolicy, RunLimits};

use super::BrokerError;

#[cfg(unix)]
use cap_std::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    ModeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub before_mode: Option<u32>,
    pub after_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatchSet {
    pub changes: Vec<FileChange>,
    pub unified_diff: String,
    pub diff_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagedFile {
    pub(super) bytes: Vec<u8>,
    pub(super) mode: u32,
}

pub(super) struct Workspace {
    _root: tempfile::TempDir,
    repo: Dir,
    source_path: PathBuf,
    filesystem: FilesystemPolicy,
    limits: RunLimits,
    excluded: GlobSet,
    baseline: BTreeMap<String, StagedFile>,
    current: BTreeMap<String, StagedFile>,
}

impl Workspace {
    pub(super) fn new(
        source_path: &Path,
        filesystem: &FilesystemPolicy,
        limits: &RunLimits,
    ) -> Result<Self, BrokerError> {
        let source_path = std::fs::canonicalize(source_path)
            .map_err(|source| BrokerError::io("open repository", "repository", source))?;
        if !source_path.is_dir() {
            return Err(BrokerError::policy(
                "repository",
                "repository root must be a directory",
            ));
        }
        if filesystem.allow_symlinks {
            return Err(BrokerError::policy(
                "filesystem.allow_symlinks",
                "the local beta intentionally supports no-follow workspaces only",
            ));
        }

        let excluded = compile_exclusions(&filesystem.excluded_paths)?;
        let source = Dir::open_ambient_dir(&source_path, ambient_authority())
            .map_err(|error| BrokerError::io("open repository", "repository", error))?;
        let baseline = snapshot_repository(&source, filesystem, &excluded)?;

        let root = tempfile::Builder::new()
            .prefix("tt-agent-")
            .tempdir()
            .map_err(|error| BrokerError::io("create run workspace", "runtime", error))?;
        let repo_path = root.path().join("repo");
        std::fs::create_dir(&repo_path)
            .map_err(|error| BrokerError::io("create run workspace", "runtime", error))?;
        let repo = Dir::open_ambient_dir(&repo_path, ambient_authority())
            .map_err(|error| BrokerError::io("open run workspace", "runtime", error))?;
        materialize(&repo, &baseline)?;

        Ok(Self {
            _root: root,
            repo,
            source_path,
            filesystem: filesystem.clone(),
            limits: limits.clone(),
            excluded,
            current: baseline.clone(),
            baseline,
        })
    }

    pub(super) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(super) fn workspace_path(&self) -> PathBuf {
        self._root.path().join("repo")
    }

    pub(super) fn baseline_sha256(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"tokentrimmer-agent-repository-snapshot:v1\0");
        for (path, file) in &self.baseline {
            digest.update((path.len() as u64).to_be_bytes());
            digest.update(path.as_bytes());
            digest.update(file.mode.to_be_bytes());
            digest.update((file.bytes.len() as u64).to_be_bytes());
            digest.update(&file.bytes);
        }
        hex::encode(digest.finalize())
    }

    pub(super) fn read_text(&self, raw_path: &str) -> Result<String, BrokerError> {
        let path = normalize_relative_path(raw_path)?;
        self.require_readable(&path)?;
        self.require_not_excluded(&path)?;
        ensure_no_symlink_components(&self.repo, &path, false)?;
        let file = read_file_nofollow(&self.repo, &path, self.filesystem.max_file_bytes)?;
        String::from_utf8(file.bytes)
            .map_err(|_| BrokerError::policy("read_file", "file is not UTF-8 text"))
    }

    pub(super) fn list_files(&self, raw_path: &str) -> Result<Vec<String>, BrokerError> {
        let path = normalize_relative_path(raw_path)?;
        self.require_readable(&path)?;
        self.require_not_excluded(&path)?;
        let prefix = if path == "." {
            String::new()
        } else {
            format!("{path}/")
        };
        Ok(self
            .current
            .keys()
            .filter(|candidate| *candidate == &path || candidate.starts_with(&prefix))
            .cloned()
            .collect())
    }

    pub(super) fn write_text(
        &mut self,
        raw_path: &str,
        content: String,
    ) -> Result<PatchSet, BrokerError> {
        let path = normalize_relative_path(raw_path)?;
        self.require_writable(&path)?;
        self.require_not_excluded(&path)?;
        if content.len() as u64 > self.filesystem.max_file_bytes {
            return Err(BrokerError::policy(
                "filesystem.max_file_bytes",
                format!(
                    "write contains {} bytes; ceiling is {}",
                    content.len(),
                    self.filesystem.max_file_bytes
                ),
            ));
        }
        ensure_no_symlink_components(&self.repo, &path, true)?;

        let mode = self.current.get(&path).map_or(0o644, |file| file.mode);
        let old = self.current.insert(
            path.clone(),
            StagedFile {
                bytes: content.into_bytes(),
                mode,
            },
        );
        let patch = match self.validate_candidate(&self.current) {
            Ok(patch) => patch,
            Err(error) => {
                match old {
                    Some(file) => {
                        self.current.insert(path, file);
                    }
                    None => {
                        self.current.remove(&path);
                    }
                }
                return Err(error);
            }
        };

        let staged = self.current.get(&path).expect("staged file must exist");
        if let Err(error) = write_file_nofollow(&self.repo, &path, staged) {
            match old {
                Some(file) => {
                    self.current.insert(path, file);
                }
                None => {
                    self.current.remove(&path);
                }
            }
            return Err(error);
        }
        Ok(patch)
    }

    pub(super) fn command_workspace(
        &self,
    ) -> Result<(tempfile::TempDir, PathBuf, PathBuf), BrokerError> {
        let command_root = tempfile::Builder::new()
            .prefix("command-")
            .tempdir()
            .map_err(|error| BrokerError::io("create command workspace", "runtime", error))?;
        let command_repo = command_root.path().join("repo");
        let command_runtime = command_root.path().join("runtime");
        std::fs::create_dir(&command_repo)
            .and_then(|()| std::fs::create_dir(&command_runtime))
            .map_err(|error| BrokerError::io("create command workspace", "runtime", error))?;
        let dir = Dir::open_ambient_dir(&command_repo, ambient_authority())
            .map_err(|error| BrokerError::io("open command workspace", "runtime", error))?;
        materialize(&dir, &self.current)?;
        Ok((command_root, command_repo, command_runtime))
    }

    pub(super) fn accept_command_workspace(
        &mut self,
        command_repo: &Path,
        remaining_write_bytes: u64,
    ) -> Result<(PatchSet, u64), BrokerError> {
        let dir = Dir::open_ambient_dir(command_repo, ambient_authority())
            .map_err(|error| BrokerError::io("open command result", "runtime", error))?;
        let candidate = snapshot_command_workspace(
            &dir,
            &self.baseline,
            &self.filesystem,
            &self.excluded,
            self.limits.max_changed_files,
        )?;
        let patch = self.validate_candidate(&candidate)?;
        if patch
            .changes
            .iter()
            .any(|change| change.kind == FileChangeKind::Deleted)
        {
            return Err(BrokerError::policy(
                "approvals.destructive_operations",
                "file deletion is not exposed by the local beta broker",
            ));
        }
        let write_bytes = changed_write_bytes(&self.current, &candidate)?;
        if write_bytes > remaining_write_bytes {
            return Err(BrokerError::policy(
                "filesystem.max_total_write_bytes",
                format!(
                    "command staged {write_bytes} bytes; remaining ceiling is {remaining_write_bytes}"
                ),
            ));
        }
        apply_candidate(&self.repo, &self.current, &candidate)?;
        self.current = candidate;
        Ok((patch, write_bytes))
    }

    pub(super) fn patch(&self) -> Result<PatchSet, BrokerError> {
        self.validate_candidate(&self.current)
    }

    fn validate_candidate(
        &self,
        candidate: &BTreeMap<String, StagedFile>,
    ) -> Result<PatchSet, BrokerError> {
        let patch = build_patch(&self.baseline, candidate)?;
        if patch.changes.len() as u32 > self.limits.max_changed_files {
            return Err(BrokerError::policy(
                "limits.max_changed_files",
                format!(
                    "patch changes {} files; ceiling is {}",
                    patch.changes.len(),
                    self.limits.max_changed_files
                ),
            ));
        }
        if patch.diff_bytes > self.limits.max_diff_bytes {
            return Err(BrokerError::policy(
                "limits.max_diff_bytes",
                format!(
                    "patch contains {} bytes; ceiling is {}",
                    patch.diff_bytes, self.limits.max_diff_bytes
                ),
            ));
        }
        Ok(patch)
    }

    fn require_readable(&self, path: &str) -> Result<(), BrokerError> {
        require_under_roots(
            path,
            &self.filesystem.readable_roots,
            "filesystem.readable_roots",
        )
    }

    fn require_writable(&self, path: &str) -> Result<(), BrokerError> {
        require_under_roots(
            path,
            &self.filesystem.writable_roots,
            "filesystem.writable_roots",
        )
    }

    fn require_not_excluded(&self, path: &str) -> Result<(), BrokerError> {
        if is_excluded(&self.excluded, path) {
            return Err(BrokerError::policy(
                "filesystem.excluded_paths",
                format!("path {path:?} is excluded"),
            ));
        }
        Ok(())
    }
}

fn snapshot_repository(
    source: &Dir,
    policy: &FilesystemPolicy,
    excluded: &GlobSet,
) -> Result<BTreeMap<String, StagedFile>, BrokerError> {
    let mut paths = BTreeSet::new();
    let mut scan_budget = ScanBudget::new(policy.max_files);
    for root in &policy.readable_roots {
        let root = normalize_relative_path(root)?;
        collect_paths(source, &root, excluded, &mut paths, &mut scan_budget)?;
    }

    if paths.len() as u64 > policy.max_files {
        return Err(BrokerError::policy(
            "filesystem.max_files",
            format!(
                "repository snapshot contains {} files; ceiling is {}",
                paths.len(),
                policy.max_files
            ),
        ));
    }

    let mut total = 0u64;
    let mut files = BTreeMap::new();
    for path in paths {
        let file = read_file_nofollow(source, &path, policy.max_file_bytes)?;
        total = total.checked_add(file.bytes.len() as u64).ok_or_else(|| {
            BrokerError::policy("filesystem.max_total_read_bytes", "byte count overflow")
        })?;
        if total > policy.max_total_read_bytes {
            return Err(BrokerError::policy(
                "filesystem.max_total_read_bytes",
                format!(
                    "repository snapshot contains {total} bytes; ceiling is {}",
                    policy.max_total_read_bytes
                ),
            ));
        }
        files.insert(path, file);
    }
    Ok(files)
}

fn snapshot_command_workspace(
    dir: &Dir,
    baseline: &BTreeMap<String, StagedFile>,
    policy: &FilesystemPolicy,
    excluded: &GlobSet,
    max_changed_files: u32,
) -> Result<BTreeMap<String, StagedFile>, BrokerError> {
    let max_files = policy
        .max_files
        .saturating_add(u64::from(max_changed_files));
    let mut paths = BTreeSet::new();
    let mut scan_budget = ScanBudget::new(max_files);
    collect_paths(dir, ".", excluded, &mut paths, &mut scan_budget)?;
    if paths.len() as u64 > max_files {
        return Err(BrokerError::policy(
            "filesystem.max_files",
            format!(
                "command workspace contains {} files; snapshot-plus-change ceiling is {max_files}",
                paths.len()
            ),
        ));
    }

    let baseline_bytes: u64 = baseline.values().map(|file| file.bytes.len() as u64).sum();
    let scan_byte_ceiling = baseline_bytes.saturating_add(policy.max_total_write_bytes);
    let mut total = 0u64;
    let mut files = BTreeMap::new();
    for path in paths {
        if !is_under_any_root(&path, &policy.readable_roots)
            && !is_under_any_root(&path, &policy.writable_roots)
        {
            return Err(BrokerError::policy(
                "filesystem roots",
                format!("command created inaccessible path {path:?}"),
            ));
        }
        let file = read_file_nofollow(dir, &path, policy.max_file_bytes)?;
        total = total.checked_add(file.bytes.len() as u64).ok_or_else(|| {
            BrokerError::policy("filesystem.max_total_write_bytes", "byte count overflow")
        })?;
        if total > scan_byte_ceiling {
            return Err(BrokerError::policy(
                "filesystem.max_total_write_bytes",
                format!("command workspace exceeds {scan_byte_ceiling} bytes"),
            ));
        }
        files.insert(path, file);
    }

    for (path, before) in baseline {
        if files.get(path).is_some_and(|after| after != before)
            && !is_under_any_root(path, &policy.writable_roots)
        {
            return Err(BrokerError::policy(
                "filesystem.writable_roots",
                format!("command changed read-only path {path:?}"),
            ));
        }
    }
    for path in files.keys() {
        if !baseline.contains_key(path) && !is_under_any_root(path, &policy.writable_roots) {
            return Err(BrokerError::policy(
                "filesystem.writable_roots",
                format!("command created path {path:?} outside writable roots"),
            ));
        }
    }
    Ok(files)
}

struct ScanBudget {
    max_files: u64,
    max_entries: u64,
    entries: u64,
}

impl ScanBudget {
    fn new(max_files: u64) -> Self {
        Self {
            max_files,
            max_entries: max_files.saturating_mul(8).saturating_add(1_024),
            entries: 0,
        }
    }

    fn observe_entry(&mut self) -> Result<(), BrokerError> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > self.max_entries {
            return Err(BrokerError::policy(
                "filesystem.max_files",
                format!(
                    "repository scan exceeded {} directory entries",
                    self.max_entries
                ),
            ));
        }
        Ok(())
    }

    fn observe_file(&self, file_count: usize) -> Result<(), BrokerError> {
        if file_count as u64 > self.max_files {
            return Err(BrokerError::policy(
                "filesystem.max_files",
                format!(
                    "repository snapshot contains {file_count} files; ceiling is {}",
                    self.max_files
                ),
            ));
        }
        Ok(())
    }
}

fn collect_paths(
    dir: &Dir,
    raw_path: &str,
    excluded: &GlobSet,
    out: &mut BTreeSet<String>,
    scan_budget: &mut ScanBudget,
) -> Result<(), BrokerError> {
    let path = normalize_relative_path(raw_path)?;
    if path != "." && is_excluded(excluded, &path) {
        return Ok(());
    }
    let metadata = dir
        .symlink_metadata(&path)
        .map_err(|error| BrokerError::io("inspect path", &path, error))?;
    if metadata.is_symlink() {
        return Err(BrokerError::policy(
            "filesystem.allow_symlinks",
            format!("symlinked path {path:?} is refused"),
        ));
    }
    if metadata.is_file() {
        out.insert(path);
        scan_budget.observe_file(out.len())?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(BrokerError::policy(
            "filesystem",
            format!("non-regular path {path:?} is refused"),
        ));
    }

    let mut children = Vec::new();
    let entries = dir
        .read_dir(&path)
        .map_err(|error| BrokerError::io("list directory", &path, error))?;
    for entry in entries {
        scan_budget.observe_entry()?;
        let entry = entry.map_err(|error| BrokerError::io("list directory", &path, error))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| BrokerError::policy("filesystem", "non-UTF-8 path names are refused"))?;
        let child = if path == "." {
            name
        } else {
            format!("{path}/{name}")
        };
        children.push(child);
    }
    children.sort();
    for child in children {
        collect_paths(dir, &child, excluded, out, scan_budget)?;
    }
    Ok(())
}

fn read_file_nofollow(dir: &Dir, path: &str, max_bytes: u64) -> Result<StagedFile, BrokerError> {
    let before = dir
        .canonicalize(path)
        .map_err(|error| BrokerError::io("resolve file", path, error))?;
    if path_string(&before)? != path {
        return Err(BrokerError::policy(
            "filesystem.allow_symlinks",
            format!("path {path:?} resolves through a symlink"),
        ));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = dir
        .open_with(path, &options)
        .map_err(|error| BrokerError::io("open file", path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| BrokerError::io("inspect open file", path, error))?;
    if !opened.is_file() {
        return Err(BrokerError::policy(
            "filesystem",
            format!("path {path:?} is not a regular file"),
        ));
    }
    #[cfg(unix)]
    if opened.nlink() > 1 {
        return Err(BrokerError::policy(
            "filesystem",
            format!("hard-linked file {path:?} is refused"),
        ));
    }
    if opened.len() > max_bytes {
        return Err(BrokerError::policy(
            "filesystem.max_file_bytes",
            format!(
                "file {path:?} contains {} bytes; ceiling is {max_bytes}",
                opened.len()
            ),
        ));
    }

    let capacity = usize::try_from(opened.len()).map_err(|_| {
        BrokerError::policy("filesystem.max_file_bytes", "file size does not fit memory")
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| BrokerError::io("read file", path, error))?;
    if bytes.len() as u64 > max_bytes {
        return Err(BrokerError::policy(
            "filesystem.max_file_bytes",
            format!("file {path:?} grew above {max_bytes} bytes while reading"),
        ));
    }

    let after = dir
        .canonicalize(path)
        .map_err(|error| BrokerError::io("re-resolve file", path, error))?;
    if path_string(&after)? != path {
        return Err(BrokerError::policy(
            "filesystem.allow_symlinks",
            format!("path {path:?} changed identity while reading"),
        ));
    }
    let after_metadata = dir
        .metadata(&after)
        .map_err(|error| BrokerError::io("re-inspect file", path, error))?;
    if !same_identity(&opened, &after_metadata) {
        return Err(BrokerError::policy(
            "filesystem",
            format!("path {path:?} changed identity while reading"),
        ));
    }

    Ok(StagedFile {
        bytes,
        mode: file_mode(&opened),
    })
}

fn write_file_nofollow(dir: &Dir, path: &str, file: &StagedFile) -> Result<(), BrokerError> {
    let target = Path::new(path);
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if parent != Path::new(".") {
        dir.create_dir_all(parent)
            .map_err(|error| BrokerError::io("create parent directory", path, error))?;
    }
    ensure_no_symlink_components(dir, path, true)?;
    let temporary = parent.join(format!(".tt-stage-{}", Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut handle = dir
            .open_with(&temporary, &options)
            .map_err(|error| BrokerError::io("create atomic staged file", path, error))?;
        handle
            .write_all(&file.bytes)
            .map_err(|error| BrokerError::io("write atomic staged file", path, error))?;
        #[cfg(unix)]
        handle
            .set_permissions(cap_std::fs::Permissions::from_mode(file.mode))
            .map_err(|error| BrokerError::io("set staged file mode", path, error))?;
        handle
            .sync_all()
            .map_err(|error| BrokerError::io("sync staged file", path, error))?;
        drop(handle);
        dir.rename(&temporary, dir, path)
            .map_err(|error| BrokerError::io("commit atomic staged file", path, error))
    })();
    if result.is_err() {
        let _ = dir.remove_file(&temporary);
    }
    result
}

fn materialize(dir: &Dir, files: &BTreeMap<String, StagedFile>) -> Result<(), BrokerError> {
    for (path, file) in files {
        write_file_nofollow(dir, path, file)?;
    }
    Ok(())
}

fn apply_candidate(
    dir: &Dir,
    current: &BTreeMap<String, StagedFile>,
    candidate: &BTreeMap<String, StagedFile>,
) -> Result<(), BrokerError> {
    for path in current.keys().filter(|path| !candidate.contains_key(*path)) {
        dir.remove_file(path)
            .map_err(|error| BrokerError::io("remove staged file", path, error))?;
    }
    for (path, file) in candidate {
        if current.get(path) != Some(file) {
            write_file_nofollow(dir, path, file)?;
        }
    }
    Ok(())
}

fn changed_write_bytes(
    current: &BTreeMap<String, StagedFile>,
    candidate: &BTreeMap<String, StagedFile>,
) -> Result<u64, BrokerError> {
    candidate
        .iter()
        .filter(|(path, file)| current.get(*path) != Some(*file))
        .try_fold(0u64, |total, (_, file)| {
            total.checked_add(file.bytes.len() as u64).ok_or_else(|| {
                BrokerError::policy("filesystem.max_total_write_bytes", "byte count overflow")
            })
        })
}

fn build_patch(
    baseline: &BTreeMap<String, StagedFile>,
    current: &BTreeMap<String, StagedFile>,
) -> Result<PatchSet, BrokerError> {
    let paths: BTreeSet<_> = baseline.keys().chain(current.keys()).cloned().collect();
    let mut changes = Vec::new();
    let mut unified_diff = String::new();

    for path in paths {
        let before = baseline.get(&path);
        let after = current.get(&path);
        if before == after {
            continue;
        }
        let kind = match (before, after) {
            (None, Some(_)) => FileChangeKind::Added,
            (Some(_), None) => FileChangeKind::Deleted,
            (Some(a), Some(b)) if a.bytes == b.bytes => FileChangeKind::ModeChanged,
            (Some(_), Some(_)) => FileChangeKind::Modified,
            (None, None) => continue,
        };
        changes.push(FileChange {
            path: path.clone(),
            kind,
            before_sha256: before.map(|file| sha256_hex(&file.bytes)),
            after_sha256: after.map(|file| sha256_hex(&file.bytes)),
            before_mode: before.map(|file| file.mode),
            after_mode: after.map(|file| file.mode),
        });

        if before.map(|file| file.mode) != after.map(|file| file.mode) {
            if let Some(mode) = before.map(|file| file.mode) {
                unified_diff.push_str(&format!("old mode {mode:06o}\n"));
            }
            if let Some(mode) = after.map(|file| file.mode) {
                unified_diff.push_str(&format!("new mode {mode:06o}\n"));
            }
        }
        if before.map(|file| &file.bytes) != after.map(|file| &file.bytes) {
            let old = before.map_or(Ok(""), |file| std::str::from_utf8(&file.bytes));
            let new = after.map_or(Ok(""), |file| std::str::from_utf8(&file.bytes));
            let (old, new) = match (old, new) {
                (Ok(old), Ok(new)) => (old, new),
                _ => {
                    return Err(BrokerError::policy(
                        "limits.max_diff_bytes",
                        format!("binary change to {path:?} is unsupported"),
                    ))
                }
            };
            let old_header = if before.is_some() {
                format!("a/{path}")
            } else {
                "/dev/null".into()
            };
            let new_header = if after.is_some() {
                format!("b/{path}")
            } else {
                "/dev/null".into()
            };
            let diff = TextDiff::from_lines(old, new)
                .unified_diff()
                .context_radius(3)
                .header(&old_header, &new_header)
                .to_string();
            unified_diff.push_str(&diff);
        }
    }

    Ok(PatchSet {
        diff_bytes: unified_diff.len() as u64,
        changes,
        unified_diff,
    })
}

fn compile_exclusions(patterns: &[String]) -> Result<GlobSet, BrokerError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| {
                BrokerError::policy(
                    "filesystem.excluded_paths",
                    format!("invalid pattern {pattern:?}: {error}"),
                )
            })?;
        builder.add(glob);
    }
    builder.build().map_err(|error| {
        BrokerError::policy(
            "filesystem.excluded_paths",
            format!("failed to compile patterns: {error}"),
        )
    })
}

pub(super) fn normalize_relative_path(raw: &str) -> Result<String, BrokerError> {
    if raw.is_empty()
        || raw.contains('\\')
        || raw.chars().any(|ch| ch == '\0' || ch.is_control())
        || Path::new(raw).is_absolute()
    {
        return Err(BrokerError::policy(
            "path",
            "path must be a canonical repository-relative UTF-8 path",
        ));
    }
    if raw == "." {
        return Ok(".".into());
    }
    let mut normalized = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(value) => normalized.push(
                value
                    .to_str()
                    .ok_or_else(|| BrokerError::policy("path", "path must be UTF-8"))?,
            ),
            _ => {
                return Err(BrokerError::policy(
                    "path",
                    "path must not contain parent, current, root, or prefix components",
                ))
            }
        }
    }
    let result = normalized.join("/");
    if result != raw {
        return Err(BrokerError::policy(
            "path",
            "path must already be canonical",
        ));
    }
    Ok(result)
}

fn path_string(path: &Path) -> Result<String, BrokerError> {
    let text = path
        .to_str()
        .ok_or_else(|| BrokerError::policy("filesystem", "non-UTF-8 path is refused"))?;
    Ok(if text.is_empty() { "." } else { text }.replace('\\', "/"))
}

fn require_under_roots(
    path: &str,
    roots: &[String],
    field: &'static str,
) -> Result<(), BrokerError> {
    if is_under_any_root(path, roots) {
        Ok(())
    } else {
        Err(BrokerError::policy(
            field,
            format!("path {path:?} is outside authorized roots"),
        ))
    }
}

fn is_under_any_root(path: &str, roots: &[String]) -> bool {
    roots.iter().any(|root| is_under_root(path, root))
}

fn is_under_root(path: &str, root: &str) -> bool {
    root == "."
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_excluded(excluded: &GlobSet, path: &str) -> bool {
    excluded.is_match(path) || excluded.is_match(format!("{path}/"))
}

fn ensure_no_symlink_components(
    dir: &Dir,
    path: &str,
    allow_missing_final: bool,
) -> Result<(), BrokerError> {
    let components: Vec<_> = Path::new(path).components().collect();
    let mut prefix = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component.as_os_str());
        match dir.symlink_metadata(&prefix) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err(BrokerError::policy(
                    "filesystem.allow_symlinks",
                    format!("symlinked path {:?} is refused", path_string(&prefix)?),
                ))
            }
            Ok(metadata) if index + 1 < components.len() && !metadata.is_dir() => {
                return Err(BrokerError::policy(
                    "path",
                    format!("non-directory path component {:?}", path_string(&prefix)?),
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing_final => {
                break;
            }
            Err(error) => {
                return Err(BrokerError::io(
                    "inspect path component",
                    &path_string(&prefix)?,
                    error,
                ))
            }
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(unix)]
fn same_identity(a: &cap_std::fs::Metadata, b: &cap_std::fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_identity(a: &cap_std::fs::Metadata, b: &cap_std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

#[cfg(unix)]
fn file_mode(metadata: &cap_std::fs::Metadata) -> u32 {
    metadata.mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(metadata: &cap_std::fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}
