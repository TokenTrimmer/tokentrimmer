use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const ENDPOINT_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    Probe,
    OperatorOverride,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityValue {
    pub supported: Option<bool>,
    pub source: CapabilitySource,
}

impl CapabilityValue {
    pub const fn unknown() -> Self {
        Self {
            supported: None,
            source: CapabilitySource::Unknown,
        }
    }

    pub const fn probed(supported: bool) -> Self {
        Self {
            supported: Some(supported),
            source: CapabilitySource::Probe,
        }
    }

    pub const fn overridden(supported: bool) -> Self {
        Self {
            supported: Some(supported),
            source: CapabilitySource::OperatorOverride,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCapabilities {
    pub models: CapabilityValue,
    pub chat: CapabilityValue,
    pub streaming: CapabilityValue,
    pub tools: CapabilityValue,
    pub json_schema: CapabilityValue,
    pub embeddings: CapabilityValue,
    pub multimodal_input: CapabilityValue,
    pub cancellation: CapabilityValue,
    pub usage_reporting: CapabilityValue,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl Default for EndpointCapabilities {
    fn default() -> Self {
        Self {
            models: CapabilityValue::unknown(),
            chat: CapabilityValue::unknown(),
            streaming: CapabilityValue::unknown(),
            tools: CapabilityValue::unknown(),
            json_schema: CapabilityValue::unknown(),
            embeddings: CapabilityValue::unknown(),
            multimodal_input: CapabilityValue::unknown(),
            cancellation: CapabilityValue::unknown(),
            usage_reporting: CapabilityValue::unknown(),
            context_length: None,
            max_output_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityOverrides {
    pub models: Option<bool>,
    pub chat: Option<bool>,
    pub streaming: Option<bool>,
    pub tools: Option<bool>,
    pub json_schema: Option<bool>,
    pub embeddings: Option<bool>,
    pub multimodal_input: Option<bool>,
    pub cancellation: Option<bool>,
    pub usage_reporting: Option<bool>,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl CapabilityOverrides {
    pub fn apply(&self, capabilities: &mut EndpointCapabilities) {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    capabilities.$field = CapabilityValue::overridden(value);
                }
            };
        }
        apply!(models);
        apply!(chat);
        apply!(streaming);
        apply!(tools);
        apply!(json_schema);
        apply!(embeddings);
        apply!(multimodal_input);
        apply!(cancellation);
        apply!(usage_reporting);
        if self.context_length.is_some() {
            capabilities.context_length = self.context_length;
        }
        if self.max_output_tokens.is_some() {
            capabilities.max_output_tokens = self.max_output_tokens;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointDiscoveryConfig {
    #[serde(default = "default_freshness_seconds")]
    pub freshness_seconds: u64,
    #[serde(default)]
    pub require_fresh: bool,
    #[serde(default)]
    pub active_probe: bool,
    #[serde(default = "default_probe_timeout_ms")]
    pub probe_timeout_ms: u64,
    #[serde(default)]
    pub probe_model: Option<String>,
    #[serde(default)]
    pub embedding_probe_model: Option<String>,
    #[serde(default)]
    pub overrides: CapabilityOverrides,
}

const fn default_freshness_seconds() -> u64 {
    300
}
const fn default_probe_timeout_ms() -> u64 {
    30_000
}

impl Default for EndpointDiscoveryConfig {
    fn default() -> Self {
        Self {
            freshness_seconds: default_freshness_seconds(),
            require_fresh: false,
            active_probe: false,
            probe_timeout_ms: default_probe_timeout_ms(),
            probe_model: None,
            embedding_probe_model: None,
            overrides: CapabilityOverrides::default(),
        }
    }
}

impl EndpointDiscoveryConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.freshness_seconds == 0
            || self.probe_timeout_ms == 0
            || self
                .probe_model
                .as_deref()
                .is_some_and(|model| model.trim().is_empty())
            || self
                .embedding_probe_model
                .as_deref()
                .is_some_and(|model| model.trim().is_empty())
        {
            return Err(RuntimeError::InvalidDiscovery);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointNetworkScope {
    #[default]
    Loopback,
    Private,
    External,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointPrivacyConfig {
    #[serde(default)]
    pub network_scope: EndpointNetworkScope,
    #[serde(default)]
    pub residency: Option<String>,
}

impl EndpointPrivacyConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.residency.as_deref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                || value.starts_with('-')
                || value.ends_with('-')
        }) {
            return Err(RuntimeError::InvalidPrivacy);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointHealth {
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointCapabilitySnapshot {
    pub schema_version: u32,
    pub profile: String,
    pub config_revision_sha256: String,
    pub checked_at: DateTime<Utc>,
    pub fresh_until: DateTime<Utc>,
    pub health: EndpointHealth,
    pub engine_version: Option<String>,
    pub model_ids: Vec<String>,
    pub capabilities: EndpointCapabilities,
    pub last_error: Option<String>,
}

impl EndpointCapabilitySnapshot {
    #[must_use]
    pub fn is_fresh_at(&self, now: DateTime<Utc>) -> bool {
        self.schema_version == ENDPOINT_STATE_SCHEMA_VERSION && self.fresh_until >= now
    }

    /// Probe an HTTP endpoint and return a fresh capability snapshot.
    pub async fn probe(
        profile_name: &str,
        base_url: &str,
        api_key: Option<&str>,
        config: &EndpointDiscoveryConfig,
        config_sha256: &str,
    ) -> Self {
        let now = Utc::now();
        let fresh_until = now + chrono::Duration::seconds(config.freshness_seconds as i64);
        let mut snapshot = Self {
            schema_version: ENDPOINT_STATE_SCHEMA_VERSION,
            profile: profile_name.to_owned(),
            config_revision_sha256: config_sha256.to_owned(),
            checked_at: now,
            fresh_until,
            health: EndpointHealth::Healthy,
            engine_version: None,
            model_ids: Vec::new(),
            capabilities: EndpointCapabilities::default(),
            last_error: None,
        };

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(config.probe_timeout_ms))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                snapshot.health = EndpointHealth::Unhealthy;
                snapshot.last_error = Some(e.to_string());
                return snapshot;
            }
        };

        let base = base_url.trim_end_matches('/');
        let models_url = format!("{base}/models");
        let mut req = client.get(&models_url);
        if let Some(key) = api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                snapshot.capabilities.models = CapabilityValue::probed(true);
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
                        for m in data {
                            if let Some(id) = m.get("id").and_then(|id| id.as_str()) {
                                snapshot.model_ids.push(id.to_owned());
                            }
                        }
                    }
                }
                snapshot.capabilities.chat = CapabilityValue::probed(true);
                snapshot.capabilities.streaming = CapabilityValue::probed(true);
            }
            Ok(resp) => {
                snapshot.capabilities.models = CapabilityValue::probed(false);
                snapshot.last_error = Some(format!("HTTP {}", resp.status()));
            }
            Err(e) => {
                snapshot.health = EndpointHealth::Unhealthy;
                snapshot.capabilities.models = CapabilityValue::probed(false);
                snapshot.last_error = Some(e.to_string());
            }
        }

        config.overrides.apply(&mut snapshot.capabilities);
        snapshot
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedEndpointState {
    schema_version: u32,
    profiles: BTreeMap<String, EndpointCapabilitySnapshot>,
}

#[derive(Debug, Clone)]
pub struct EndpointStateStore {
    path: PathBuf,
}

impl EndpointStateStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<BTreeMap<String, EndpointCapabilitySnapshot>, RuntimeError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new())
            }
            Err(error) => return Err(RuntimeError::StateIo(error)),
        };
        let state: PersistedEndpointState = serde_json::from_slice(&bytes)?;
        if state.schema_version != ENDPOINT_STATE_SCHEMA_VERSION
            || state
                .profiles
                .values()
                .any(|snapshot| snapshot.schema_version != ENDPOINT_STATE_SCHEMA_VERSION)
        {
            return Err(RuntimeError::StateSchema(state.schema_version));
        }
        Ok(state.profiles)
    }

    pub fn save(
        &self,
        profiles: &BTreeMap<String, EndpointCapabilitySnapshot>,
    ) -> Result<(), RuntimeError> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(RuntimeError::MissingStateParent(parent.to_path_buf()));
        }
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(RuntimeError::StateSymlink(self.path.clone()));
        }
        let payload = serde_json::to_vec_pretty(&PersistedEndpointState {
            schema_version: ENDPOINT_STATE_SCHEMA_VERSION,
            profiles: profiles.clone(),
        })?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| RuntimeError::InvalidStatePath(self.path.clone()))?;
        let temporary = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let write_result = (|| -> Result<(), RuntimeError> {
            let mut file = options.open(&temporary)?;
            file.write_all(&payload)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTcoProfile {
    pub revision: String,
    #[serde(default)]
    pub hardware_purchase_micros: u64,
    #[serde(default)]
    pub hardware_lease_micros_per_hour: u64,
    pub amortization_hours: u64,
    pub active_power_watts: u32,
    pub idle_power_watts: u32,
    pub electricity_micros_per_kwh: u64,
    pub utilization_basis_points: u16,
    #[serde(default)]
    pub hosting_micros_per_hour: u64,
    #[serde(default)]
    pub operator_micros_per_hour: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTcoEvidence {
    pub profile_revision: String,
    pub profile_sha256: String,
    pub duration_ms: u64,
    pub marginal_energy_micros: u64,
    pub allocated_energy_micros: u64,
    pub hardware_amortization_micros: u64,
    pub hardware_lease_micros: u64,
    pub hosting_micros: u64,
    pub operator_micros: u64,
    pub total_allocated_micros: u64,
}

impl LocalTcoProfile {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.revision.trim().is_empty()
            || self.amortization_hours == 0
            || self.active_power_watts < self.idle_power_watts
            || !(1..=10_000).contains(&self.utilization_basis_points)
        {
            return Err(RuntimeError::InvalidTcoProfile);
        }
        Ok(())
    }

    pub fn estimate(&self, duration: Duration) -> Result<LocalTcoEvidence, RuntimeError> {
        self.validate()?;
        let duration_ms = u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let marginal_watts = u64::from(self.active_power_watts - self.idle_power_watts);
        let marginal_energy =
            energy_cost_micros(marginal_watts, duration_ms, self.electricity_micros_per_kwh);
        let active_energy = energy_cost_micros(
            u64::from(self.active_power_watts),
            duration_ms,
            self.electricity_micros_per_kwh,
        );
        let allocated_energy =
            allocate_for_utilization(active_energy, self.utilization_basis_points);
        let hardware_hourly = div_ceil(
            u128::from(self.hardware_purchase_micros),
            u128::from(self.amortization_hours),
        );
        let hardware_amortization =
            allocated_time_cost(hardware_hourly, duration_ms, self.utilization_basis_points);
        let hardware_lease = allocated_time_cost(
            u128::from(self.hardware_lease_micros_per_hour),
            duration_ms,
            self.utilization_basis_points,
        );
        let hosting = allocated_time_cost(
            u128::from(self.hosting_micros_per_hour),
            duration_ms,
            self.utilization_basis_points,
        );
        let operator = allocated_time_cost(
            u128::from(self.operator_micros_per_hour),
            duration_ms,
            self.utilization_basis_points,
        );
        let total = allocated_energy
            .saturating_add(hardware_amortization)
            .saturating_add(hardware_lease)
            .saturating_add(hosting)
            .saturating_add(operator);
        Ok(LocalTcoEvidence {
            profile_revision: self.revision.clone(),
            profile_sha256: sha256_json(self)?,
            duration_ms,
            marginal_energy_micros: marginal_energy,
            allocated_energy_micros: allocated_energy,
            hardware_amortization_micros: hardware_amortization,
            hardware_lease_micros: hardware_lease,
            hosting_micros: hosting,
            operator_micros: operator,
            total_allocated_micros: total,
        })
    }
}

fn energy_cost_micros(watts: u64, duration_ms: u64, micros_per_kwh: u64) -> u64 {
    to_u64(div_ceil(
        u128::from(watts)
            .saturating_mul(u128::from(duration_ms))
            .saturating_mul(u128::from(micros_per_kwh)),
        3_600_000_000,
    ))
}

fn allocated_time_cost(hourly_micros: u128, duration_ms: u64, utilization_bp: u16) -> u64 {
    to_u64(div_ceil(
        hourly_micros
            .saturating_mul(u128::from(duration_ms))
            .saturating_mul(10_000),
        3_600_000_u128.saturating_mul(u128::from(utilization_bp)),
    ))
}

fn allocate_for_utilization(value: u64, utilization_bp: u16) -> u64 {
    to_u64(div_ceil(
        u128::from(value).saturating_mul(10_000),
        u128::from(utilization_bp),
    ))
}

const fn div_ceil(numerator: u128, denominator: u128) -> u128 {
    numerator.saturating_add(denominator.saturating_sub(1)) / denominator
}

fn to_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareProvenance {
    pub profile: String,
    pub accelerator: String,
    pub memory_bytes: u64,
    pub digest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelProvenance {
    pub model_digest_sha256: String,
    pub quantization: String,
    pub tokenizer_digest_sha256: String,
    pub engine: String,
    pub engine_version: String,
    pub context_tokens: u32,
    #[serde(default)]
    pub adapter_digests_sha256: Vec<String>,
    pub hardware: HardwareProvenance,
}

impl LocalModelProvenance {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if !is_sha256(&self.model_digest_sha256)
            || !is_sha256(&self.tokenizer_digest_sha256)
            || !is_sha256(&self.hardware.digest_sha256)
            || self
                .adapter_digests_sha256
                .iter()
                .any(|digest| !is_sha256(digest))
            || self.quantization.trim().is_empty()
            || self.engine.trim().is_empty()
            || self.engine_version.trim().is_empty()
            || self.hardware.profile.trim().is_empty()
            || self.hardware.accelerator.trim().is_empty()
            || self.hardware.memory_bytes == 0
            || self.context_tokens == 0
        {
            return Err(RuntimeError::InvalidProvenance);
        }
        Ok(())
    }

    pub fn revision_sha256(&self) -> Result<String, RuntimeError> {
        self.validate()?;
        sha256_json(self)
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn sha256_json<T: Serialize>(value: &T) -> Result<String, RuntimeError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityConfig {
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default)]
    pub max_queue_depth: usize,
    #[serde(default = "default_queue_timeout_ms")]
    pub queue_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_circuit_open_ms")]
    pub circuit_open_ms: u64,
}

const fn default_max_concurrency() -> usize {
    1
}
const fn default_queue_timeout_ms() -> u64 {
    30_000
}
const fn default_request_timeout_ms() -> u64 {
    300_000
}
const fn default_failure_threshold() -> u32 {
    3
}
const fn default_circuit_open_ms() -> u64 {
    30_000
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            max_concurrency: default_max_concurrency(),
            max_queue_depth: 0,
            queue_timeout_ms: default_queue_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
            failure_threshold: default_failure_threshold(),
            circuit_open_ms: default_circuit_open_ms(),
        }
    }
}

impl CapacityConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.max_concurrency == 0
            || self.queue_timeout_ms == 0
            || self.request_timeout_ms == 0
            || self.failure_threshold == 0
            || self.circuit_open_ms == 0
        {
            return Err(RuntimeError::InvalidCapacity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapacitySnapshot {
    pub active: usize,
    pub queued: usize,
    pub max_concurrency: usize,
    pub max_queue_depth: usize,
    pub circuit_open: bool,
    pub consecutive_failures: u32,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
}

#[derive(Debug)]
struct BreakerState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

#[derive(Debug)]
pub struct AdmissionController {
    config: CapacityConfig,
    semaphore: Arc<Semaphore>,
    queued: AtomicUsize,
    breaker: Mutex<BreakerState>,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
}

impl AdmissionController {
    pub fn new(config: CapacityConfig) -> Result<Arc<Self>, RuntimeError> {
        config.validate()?;
        Ok(Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(config.max_concurrency)),
            config,
            queued: AtomicUsize::new(0),
            breaker: Mutex::new(BreakerState {
                consecutive_failures: 0,
                opened_at: None,
            }),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
        }))
    }

    pub async fn admit(self: &Arc<Self>) -> Result<AdmissionPermit, AdmissionError> {
        self.check_circuit()?;
        if let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() {
            return Ok(AdmissionPermit::new(Arc::clone(self), permit));
        }
        let queue_slot = QueueSlot::acquire(self)?;
        let acquire = Arc::clone(&self.semaphore).acquire_owned();
        let permit =
            tokio::time::timeout(Duration::from_millis(self.config.queue_timeout_ms), acquire)
                .await
                .map_err(|_| AdmissionError::QueueTimeout)?
                .map_err(|_| AdmissionError::Closed)?;
        drop(queue_slot);
        self.check_circuit()?;
        Ok(AdmissionPermit::new(Arc::clone(self), permit))
    }

    fn check_circuit(&self) -> Result<(), AdmissionError> {
        let mut breaker = self
            .breaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(opened_at) = breaker.opened_at {
            if opened_at.elapsed() < Duration::from_millis(self.config.circuit_open_ms) {
                return Err(AdmissionError::CircuitOpen);
            }
            breaker.opened_at = None;
            breaker.consecutive_failures = 0;
        }
        Ok(())
    }

    fn record_success(&self) {
        let mut breaker = self
            .breaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        breaker.consecutive_failures = 0;
        breaker.opened_at = None;
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_failure(&self) {
        let mut breaker = self
            .breaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        breaker.consecutive_failures = breaker.consecutive_failures.saturating_add(1);
        if breaker.consecutive_failures >= self.config.failure_threshold {
            breaker.opened_at = Some(Instant::now());
        }
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_cancelled(&self) {
        self.cancelled.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.config.request_timeout_ms)
    }

    #[must_use]
    pub fn snapshot(&self) -> CapacitySnapshot {
        let breaker = self
            .breaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        CapacitySnapshot {
            active: self
                .config
                .max_concurrency
                .saturating_sub(self.semaphore.available_permits()),
            queued: self.queued.load(Ordering::Relaxed),
            max_concurrency: self.config.max_concurrency,
            max_queue_depth: self.config.max_queue_depth,
            circuit_open: breaker.opened_at.is_some_and(|opened_at| {
                opened_at.elapsed() < Duration::from_millis(self.config.circuit_open_ms)
            }),
            consecutive_failures: breaker.consecutive_failures,
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
        }
    }
}

struct QueueSlot<'a> {
    controller: &'a AdmissionController,
}

impl<'a> QueueSlot<'a> {
    fn acquire(controller: &'a AdmissionController) -> Result<Self, AdmissionError> {
        let result =
            controller
                .queued
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    (queued < controller.config.max_queue_depth).then_some(queued.saturating_add(1))
                });
        result.map_err(|_| AdmissionError::QueueFull)?;
        Ok(Self { controller })
    }
}

impl Drop for QueueSlot<'_> {
    fn drop(&mut self) {
        self.controller.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct AdmissionPermit {
    controller: Arc<AdmissionController>,
    _permit: OwnedSemaphorePermit,
    terminal: bool,
}

impl AdmissionPermit {
    fn new(controller: Arc<AdmissionController>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            controller,
            _permit: permit,
            terminal: false,
        }
    }

    pub fn success(mut self) {
        self.controller.record_success();
        self.terminal = true;
    }

    pub fn failure(mut self) {
        self.controller.record_failure();
        self.terminal = true;
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        if !self.terminal {
            self.controller.record_cancelled();
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("local endpoint queue is full")]
    QueueFull,
    #[error("local endpoint queue wait timed out")]
    QueueTimeout,
    #[error("local endpoint circuit breaker is open")]
    CircuitOpen,
    #[error("local endpoint admission controller is closed")]
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("endpoint state I/O failed: {0}")]
    StateIo(#[from] std::io::Error),
    #[error("endpoint state JSON failed: {0}")]
    StateJson(#[from] serde_json::Error),
    #[error("unsupported endpoint state schema version {0}")]
    StateSchema(u32),
    #[error("endpoint state parent directory does not exist: {0}")]
    MissingStateParent(PathBuf),
    #[error("endpoint state path must not be a symbolic link: {0}")]
    StateSymlink(PathBuf),
    #[error("invalid endpoint state path: {0}")]
    InvalidStatePath(PathBuf),
    #[error("invalid local TCO profile")]
    InvalidTcoProfile,
    #[error("invalid local model or hardware provenance")]
    InvalidProvenance,
    #[error("invalid local capacity configuration")]
    InvalidCapacity,
    #[error("invalid local endpoint discovery configuration")]
    InvalidDiscovery,
    #[error("invalid local endpoint privacy configuration")]
    InvalidPrivacy,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tco_profile() -> LocalTcoProfile {
        LocalTcoProfile {
            revision: "m4-max-2026-08".into(),
            hardware_purchase_micros: 4_000_000_000,
            hardware_lease_micros_per_hour: 0,
            amortization_hours: 10_000,
            active_power_watts: 100,
            idle_power_watts: 20,
            electricity_micros_per_kwh: 250_000,
            utilization_basis_points: 5_000,
            hosting_micros_per_hour: 10_000,
            operator_micros_per_hour: 20_000,
        }
    }

    #[test]
    fn tco_keeps_marginal_energy_and_allocated_cost_separate() {
        let evidence = tco_profile().estimate(Duration::from_secs(3_600)).unwrap();
        assert_eq!(evidence.marginal_energy_micros, 20_000);
        assert_eq!(evidence.allocated_energy_micros, 50_000);
        assert_eq!(evidence.hardware_amortization_micros, 800_000);
        assert_eq!(evidence.hosting_micros, 20_000);
        assert_eq!(evidence.operator_micros, 40_000);
        assert_eq!(evidence.total_allocated_micros, 910_000);
        assert_eq!(evidence.profile_sha256.len(), 64);
    }

    #[test]
    fn endpoint_state_round_trips_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = EndpointStateStore::new(directory.path().join("state.json"));
        let now = Utc::now();
        let snapshot = EndpointCapabilitySnapshot {
            schema_version: ENDPOINT_STATE_SCHEMA_VERSION,
            profile: "gpu-a".into(),
            config_revision_sha256: "a".repeat(64),
            checked_at: now,
            fresh_until: now + chrono::Duration::seconds(300),
            health: EndpointHealth::Healthy,
            engine_version: Some("1.2.3".into()),
            model_ids: vec!["model-a".into()],
            capabilities: EndpointCapabilities::default(),
            last_error: None,
        };
        let profiles = BTreeMap::from([("gpu-a".into(), snapshot.clone())]);
        store.save(&profiles).unwrap();
        assert_eq!(store.load().unwrap().get("gpu-a"), Some(&snapshot));
    }

    #[tokio::test]
    async fn admission_enforces_queue_cancellation_and_circuit_breaker() {
        let controller = AdmissionController::new(CapacityConfig {
            max_concurrency: 1,
            max_queue_depth: 0,
            failure_threshold: 1,
            ..CapacityConfig::default()
        })
        .unwrap();
        let permit = controller.admit().await.unwrap();
        assert_eq!(
            controller.admit().await.err(),
            Some(AdmissionError::QueueFull)
        );
        drop(permit);
        assert_eq!(controller.snapshot().cancelled, 1);

        let permit = controller.admit().await.unwrap();
        permit.failure();
        assert_eq!(
            controller.admit().await.err(),
            Some(AdmissionError::CircuitOpen)
        );
    }

    #[test]
    fn provenance_requires_content_digests() {
        let provenance = LocalModelProvenance {
            model_digest_sha256: "a".repeat(64),
            quantization: "Q4_K_M".into(),
            tokenizer_digest_sha256: "b".repeat(64),
            engine: "llama.cpp".into(),
            engine_version: "b6000".into(),
            context_tokens: 32_768,
            adapter_digests_sha256: vec!["c".repeat(64)],
            hardware: HardwareProvenance {
                profile: "m4-max".into(),
                accelerator: "Apple M4 Max GPU".into(),
                memory_bytes: 128 * 1024 * 1024 * 1024,
                digest_sha256: "d".repeat(64),
            },
        };
        assert_eq!(provenance.revision_sha256().unwrap().len(), 64);
    }

    #[tokio::test]
    async fn probe_populates_capabilities_and_models() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let models_payload = serde_json::json!({
            "object": "list",
            "data": [
                { "id": "llama3:8b", "object": "model" },
                { "id": "qwen2.5:7b", "object": "model" }
            ]
        });

        let _mock = server.mock(|when, then| {
            when.method(GET).path("/models");
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(models_payload);
        });

        let config = EndpointDiscoveryConfig {
            freshness_seconds: 60,
            require_fresh: true,
            active_probe: true,
            probe_timeout_ms: 2000,
            probe_model: None,
            embedding_probe_model: None,
            overrides: CapabilityOverrides {
                tools: Some(true),
                ..CapabilityOverrides::default()
            },
        };

        let snapshot = EndpointCapabilitySnapshot::probe(
            "test-local",
            &server.base_url(),
            None,
            &config,
            &"0".repeat(64),
        )
        .await;

        assert_eq!(snapshot.health, EndpointHealth::Healthy);
        assert_eq!(snapshot.model_ids, vec!["llama3:8b", "qwen2.5:7b"]);
        assert_eq!(snapshot.capabilities.models.supported, Some(true));
        assert_eq!(snapshot.capabilities.models.source, CapabilitySource::Probe);
        assert_eq!(snapshot.capabilities.tools.supported, Some(true));
        assert_eq!(
            snapshot.capabilities.tools.source,
            CapabilitySource::OperatorOverride
        );
        assert!(snapshot.is_fresh_at(Utc::now()));
    }
}
