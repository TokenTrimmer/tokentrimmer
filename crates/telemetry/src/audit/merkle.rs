//! Merkle-tree inclusion proofs over the audit hash chain.
//!
//! The audit chain is a BLAKE3 hash chain; every entry's [`crate::audit::AuditEntry::hash`]
//! is already a 32-byte digest that binds the entry to its predecessor. The
//! Merkle layer re-uses those per-entry digests as the *raw leaves* (in `seq`
//! order) and builds an append-only binary Merkle tree over them, giving a
//! committee of an entry inside a single short root that covers a whole chain
//! prefix.
//!
//! ## Tree shape
//!
//! The tree follows the standard "canonical split" used by RFC 6962 (Certificate
//! Transparency): the root/`Merkle Tree Hash` of a contiguous range of `n > 1`
//! leaves splits at the largest power of two strictly below `n`, recursing on
//! the two halves. This keeps a deterministic, order-sensitive root for *any*
//! prefix length without padding.
//!
//! - `leaf_node(raw) = blake3(0x00 ‖ raw)`
//! - `combine(l, r)  = blake3(0x01 ‖ l ‖ r)`
//!
//! The 1-byte domain tag keeps leaves and internal nodes disjoint (defeats
//! second-preimage-style grafts between tree shapes).
//!
//! ## Streaming / bounded memory
//!
//! [`IncrementalMerkleTree::push`] appends a leaf and folds it into an O(log n)
//! frontier of perfect-subtree peaks — the live root state never grows with the
//! chain length. ([`IncrementalMerkleTree::root`] folds that frontier into the
//! canonical root.) Arbitrary-index *inclusion proofs* additionally need the
//! O(n · 32 B) leaf list, because a proof for a general index requires the
//! sibling subtree roots; the caller already holds the entries, so this is the
//! hashes only — never the JSON payloads.
//!
//! ## Scope
//!
//! This primitive is **local-only**: it proves membership of a leaf inside a
//! chain root derived from a verified export. It performs no external
//! timestamping and no transparency-log publication.

/// Version of the proof shape. Bump (and add a migration story) whenever the
/// serialized [`InclusionProof`] changes.
pub const PROOF_VERSION: u64 = 1;

/// Domain-separation tag for a leaf node.
const TAG_LEAF: u8 = 0x00;
/// Domain-separation tag for an internal node.
const TAG_INTERNAL: u8 = 0x01;

// ─── Node hashing ────────────────────────────────────────────────────────────

/// Hash a raw entry digest into a leaf *node* digest.
///
/// `raw` is the 32-byte value of an [`crate::audit::AuditEntry::hash`] field
/// (hex-decoded). The 1-byte domain tag prevents a leaf node from colliding
/// with an internal node.
pub fn leaf_node(raw: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[TAG_LEAF]);
    hasher.update(raw);
    *hasher.finalize().as_bytes()
}

/// Combine two child node digests into a parent node digest.
pub fn combine(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[TAG_INTERNAL]);
    hasher.update(left);
    hasher.update(right);
    *hasher.finalize().as_bytes()
}

/// Decode an audit entry's hex `hash` field into the 32-byte raw leaf digest
/// used by the Merkle tree.
pub fn leaf_from_hex(hex_hash: &str) -> Result<[u8; 32], MerkleError> {
    let bytes = hex::decode(hex_hash.trim())
        .map_err(|e| MerkleError::InvalidLeaf(format!("hex decode of {hex_hash:?} failed: {e}")))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| MerkleError::InvalidLeaf(format!("{hex_hash:?} is not 32 bytes")))
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors returned by [`verify_inclusion`] and related helpers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MerkleError {
    /// The proof claims a leaf that is outside its own `leaf_count`.
    #[error("leaf_index {leaf_index} is out of range for {leaf_count} leaves")]
    LeafOutOfRange { leaf_index: u64, leaf_count: u64 },
    /// The supplied root is not the root the proof anchors to (or vice versa).
    #[error("root mismatch: proof root {proof_root}, expected {expected_root}")]
    RootMismatch {
        /// Hex of the root embedded in the proof.
        proof_root: String,
        /// Hex of the root the verifier was given.
        expected_root: String,
    },
    /// The leaf does not recompute to the proof root — it is not a member.
    #[error("leaf is not a member of the tree rooted at the proof root")]
    NotMember,
    /// A leaf-hash string could not be decoded to 32 bytes.
    #[error("invalid leaf hash: {0}")]
    InvalidLeaf(String),
}

// ─── Proof types ─────────────────────────────────────────────────────────────

/// One sibling digest plus the side it sits on, hop-by-hop from the leaf up to
/// the root.
///
/// `left == true` means the sibling is the *left* child of the parent, so the
/// parent is `combine(sibling, node)`; otherwise the parent is
/// `combine(node, sibling)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sibling {
    /// The sibling node digest (a leaf node or an internal subtree root).
    pub hash: [u8; 32],
    /// `true` if the sibling is the left child at this hop.
    pub left: bool,
}

impl Sibling {
    /// Hex of the sibling node digest.
    #[must_use]
    pub fn hex(&self) -> String {
        hex::encode(self.hash)
    }
}

/// A versioned Merkle inclusion proof for one audit entry inside its chain
/// root.
///
/// It is self-describing: `leaf_index`, `leaf_count` and `root` pin down the
/// tree state the path is relative to, so a verifier does not need any chain
/// knowledge beyond `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InclusionProof {
    /// Proof shape version ([`PROOF_VERSION`]).
    pub version: u64,
    /// Stable identity of the chain the proof is about (e.g. the org UUID).
    pub chain_id: String,
    /// 0-based position of the leaf within the chain.
    pub leaf_index: u64,
    /// Total leaves the root covers (`leaf_index` is valid when `< leaf_count`).
    pub leaf_count: u64,
    /// Canonical root over all `leaf_count` leaves.
    pub root: [u8; 32],
    /// Sibling digests, ordered leaf-to-root.
    pub sibling_path: Vec<Sibling>,
}

impl InclusionProof {
    /// Hex of the root.
    #[must_use]
    pub fn root_hex(&self) -> String {
        hex::encode(self.root)
    }

    /// Session-safe one-line description (no secret material — digests only).
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "version={} chain={} leaf={}/{} path_len={} root={}",
            self.version,
            self.chain_id,
            self.leaf_index,
            self.leaf_count,
            self.sibling_path.len(),
            self.root_hex()
        )
    }
}

// ─── Tree ────────────────────────────────────────────────────────────────────

/// An append-only Merkle tree over audit entry hashes.
///
/// - `push` appends one raw leaf (an entry's `hash` field) in O(log n)
///   amortized time, touching only the O(log n) frontier peaks.
/// - `root` reconstructs the canonical root from the frontier.
/// - `prove_inclusion(i)` returns a versioned [`InclusionProof`] for leaf `i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalMerkleTree {
    /// Stable identity carried into every produced proof.
    chain_id: String,
    /// Raw leaf digests (entry `hash` fields), in `seq` order.
    leaves: Vec<[u8; 32]>,
    /// Frontier of perfect-subtree peaks: index 0 = oldest/largest, last =
    /// newest/smallest. Folding left→right reproduces the canonical root.
    frontier: Vec<[u8; 32]>,
}

impl Default for IncrementalMerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalMerkleTree {
    /// Create an empty tree with chain identity `"local"`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_chain_id("local")
    }

    /// Create an empty tree carrying `chain_id` into every proof produced.
    #[must_use]
    pub fn with_chain_id(chain_id: impl Into<String>) -> Self {
        Self {
            chain_id: chain_id.into(),
            leaves: Vec::new(),
            frontier: Vec::new(),
        }
    }

    /// Number of leaves appended so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.leaves.len() as u64
    }

    /// `true` when no leaves have been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Number of frontier peaks — the live set of subtree roots held for root
    /// reconstruction. Bounded by ⌈log2(len+1)⌉.
    #[must_use]
    pub fn frontier_peaks(&self) -> usize {
        self.frontier.len()
    }

    /// Append one raw leaf digest (an entry `hash` field).
    pub fn push(&mut self, leaf: [u8; 32]) {
        // Carry-merge: while the bit of `len` at the current level is set, the
        // just-appended node folds leftward into the existing peak at that
        // level. The popped peak is the LEFT sibling (older leaves).
        let mut node = leaf_node(&leaf);
        let mut level: u32 = 0;
        while self.leaves.len() & (1usize << level) != 0 {
            let left = self.frontier.pop().expect("frontier aligned with len");
            node = combine(&left, &node);
            level += 1;
        }
        self.frontier.push(node);
        self.leaves.push(leaf);
    }

    /// Append leaves one at a time from `iter`, returning the count pushed.
    ///
    /// This is the streaming entry point: the tree never needs the whole chain
    /// up front, only one 32-byte hash at a time, and the live root state stays
    /// O(log n).
    pub fn extend<I>(&mut self, iter: I) -> u64
    where
        I: IntoIterator<Item = [u8; 32]>,
    {
        let mut pushed = 0;
        for leaf in iter {
            self.push(leaf);
            pushed += 1;
        }
        pushed
    }

    /// The canonical root over all appended leaves, or `None` when empty.
    #[must_use]
    pub fn root(&self) -> Option<[u8; 32]> {
        fold_frontier(&self.frontier)
    }

    /// The canonical root as hex, or `None` when empty.
    #[must_use]
    pub fn root_hex(&self) -> Option<String> {
        self.root().map(hex::encode)
    }

    /// Build a versioned inclusion proof for the leaf at `leaf_index`.
    ///
    /// Returns `None` when the tree is empty or `leaf_index` is out of range.
    #[must_use]
    pub fn prove_inclusion(&self, leaf_index: u64) -> Option<InclusionProof> {
        if leaf_index >= self.len() {
            return None;
        }
        let root = self.root()?;
        let (_, sibling_path) = self.subtree_path(0, self.leaves.len(), leaf_index as usize);
        Some(InclusionProof {
            version: PROOF_VERSION,
            chain_id: self.chain_id.clone(),
            leaf_index,
            leaf_count: self.len(),
            root,
            sibling_path,
        })
    }

    /// Inclusion proof for the most recently appended leaf (the tip), if any.
    #[must_use]
    pub fn prove_tip(&self) -> Option<InclusionProof> {
        if self.is_empty() {
            None
        } else {
            self.prove_inclusion(self.len() - 1)
        }
    }

    /// Recursively compute `(subtree_root, sibling_path_bottom_up)` for the
    /// leaf at absolute index `i` inside `self.leaves[l..r]`.
    fn subtree_path(&self, l: usize, r: usize, i: usize) -> ([u8; 32], Vec<Sibling>) {
        let n = r - l;
        if n == 1 {
            debug_assert_eq!(i, l);
            return (leaf_node(&self.leaves[l]), Vec::new());
        }
        let k = largest_pow2_lt(n);
        let mid = l + k;
        if i < mid {
            let (left_root, mut path) = self.subtree_path(l, mid, i);
            let right_root = self.mth(mid, r);
            // Leaf side is the left child → sibling (right) combines to the right.
            path.push(Sibling {
                hash: right_root,
                left: false,
            });
            (combine(&left_root, &right_root), path)
        } else {
            let (right_root, mut path) = self.subtree_path(mid, r, i);
            let left_root = self.mth(l, mid);
            // Leaf side is the right child → sibling (left) combines to the left.
            path.push(Sibling {
                hash: left_root,
                left: true,
            });
            (combine(&left_root, &right_root), path)
        }
    }

    /// Merkle Tree Hash of the contiguous range `self.leaves[l..r]`.
    fn mth(&self, l: usize, r: usize) -> [u8; 32] {
        let n = r - l;
        if n == 1 {
            return leaf_node(&self.leaves[l]);
        }
        let k = largest_pow2_lt(n);
        let mid = l + k;
        combine(&self.mth(l, mid), &self.mth(mid, r))
    }
}

/// Fold frontier peaks (left→right = oldest/largest → newest/smallest) into
/// the canonical RFC-6962 root.
///
/// The MTH recursion peels the leftmost peak as the left subtree and recurses
/// on the rest, so the peaks nest **rightward**: `combine(p0, combine(p1, …))`.
/// A plain left fold would disagree whenever ≥ 3 peaks are present.
fn fold_frontier(peaks: &[[u8; 32]]) -> Option<[u8; 32]> {
    let mut iter = peaks.iter().rev();
    let rightmost = *iter.next()?;
    Some(iter.fold(rightmost, |acc, peak| combine(peak, &acc)))
}

/// Largest power of two strictly less than `n` (requires `n >= 2`).
///
/// `2^(⌊log2(n-1)⌋)` — e.g. n=5 → 4, n=8 → 4, n=9 → 8 (the RFC 6962 split).
fn largest_pow2_lt(n: usize) -> usize {
    debug_assert!(n >= 2);
    1usize << (n - 1).ilog2()
}

// ─── Verification ────────────────────────────────────────────────────────────

/// Verify that `leaf_hash` (a raw entry digest) is a member of the tree whose
/// root is `root`, using `proof`.
///
/// The proof's `leaf_index`/`leaf_count` must be internally consistent, the
/// supplied `root` must equal the proof's `root`, and replaying the sibling
/// path from the leaf must recompute that root. No external state is consulted
/// — this is a purely local membership check.
pub fn verify_inclusion(
    proof: &InclusionProof,
    leaf_hash: &[u8; 32],
    root: &[u8; 32],
) -> Result<(), MerkleError> {
    if proof.leaf_index >= proof.leaf_count {
        return Err(MerkleError::LeafOutOfRange {
            leaf_index: proof.leaf_index,
            leaf_count: proof.leaf_count,
        });
    }
    if proof.root != *root {
        return Err(MerkleError::RootMismatch {
            proof_root: hex::encode(proof.root),
            expected_root: hex::encode(*root),
        });
    }
    let mut node = leaf_node(leaf_hash);
    for sibling in &proof.sibling_path {
        node = if sibling.left {
            combine(&sibling.hash, &node)
        } else {
            combine(&node, &sibling.hash)
        };
    }
    if node == proof.root {
        Ok(())
    } else {
        Err(MerkleError::NotMember)
    }
}

/// Convenience: verify using the proof's own embedded root (the common case
/// when the root was computed from a trusted export and the proof prints on
/// top of it).
pub fn verify_inclusion_self_rooted(
    proof: &InclusionProof,
    leaf_hash: &[u8; 32],
) -> Result<(), MerkleError> {
    verify_inclusion(proof, leaf_hash, &proof.root)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random leaf for tests (blake3 of `i`'s bytes) —
    /// avoids needing an extra rand dep in the crate.
    fn leaf(i: u64) -> [u8; 32] {
        *blake3::hash(&i.to_le_bytes()).as_bytes()
    }

    fn tree_with(n: u64) -> IncrementalMerkleTree {
        let mut t = IncrementalMerkleTree::with_chain_id("test-org");
        for i in 0..n {
            t.push(leaf(i));
        }
        t
    }

    /// Naive whole-range root used as the ground truth in tests.
    fn naive_root(leaves: &[[u8; 32]]) -> Option<[u8; 32]> {
        match leaves {
            [] => None,
            [only] => Some(leaf_node(only)),
            rest => {
                let k = largest_pow2_lt(rest.len());
                let (l, r) = rest.split_at(k);
                Some(combine(&naive_root(l).unwrap(), &naive_root(r).unwrap()))
            }
        }
    }

    #[test]
    fn incremental_root_matches_batch_and_naive() {
        // "incremental root stability": pushing one leaf at a time yields the
        // same root as a batch build and as the naive whole-range computation.
        for n in 0..=48u64 {
            let leaves: Vec<[u8; 32]> = (0..n).map(leaf).collect();
            let incremental = tree_with(n);
            let mut batch = IncrementalMerkleTree::with_chain_id("batch");
            assert_eq!(batch.extend(leaves.iter().copied()), n);
            assert_eq!(incremental.len(), n);
            assert_eq!(incremental.root(), naive_root(&leaves));
            assert_eq!(incremental.root(), batch.root());
        }
        assert_eq!(IncrementalMerkleTree::new().root(), None);
    }

    #[test]
    fn ordering_is_sensitive() {
        // Two trees over the same multiset in different orders must differ —
        // the root is an ordered commitment, not a bag hash.
        let a = tree_with(8);
        let b_leaves: Vec<[u8; 32]> = (0..8u64).map(leaf).collect();
        let mut b = IncrementalMerkleTree::with_chain_id("reversed");
        b.extend(b_leaves.iter().rev().copied());
        assert_ne!(a.root(), b.root());
        // And the reversed tree still matches its own naive recomputation.
        let rev: Vec<[u8; 32]> = b_leaves.into_iter().rev().collect();
        assert_eq!(b.root(), naive_root(&rev));
    }

    #[test]
    fn prove_and_verify_round_trip_for_all_sizes_and_indexes() {
        for n in 1..=40u64 {
            let t = tree_with(n);
            let root = t.root().unwrap();
            for i in 0..n {
                let proof = t.prove_inclusion(i).expect("in range");
                assert_eq!(proof.version, PROOF_VERSION);
                assert_eq!(proof.leaf_index, i);
                assert_eq!(proof.leaf_count, n);
                assert_eq!(proof.root, root);
                assert_eq!(proof.chain_id, "test-org");
                assert!(verify_inclusion(&proof, &leaf(i), &root).is_ok());
                assert!(verify_inclusion_self_rooted(&proof, &leaf(i)).is_ok());
            }
        }
    }

    #[test]
    fn verify_rejects_wrong_leaf() {
        let t = tree_with(8);
        let proof = t.prove_inclusion(2).unwrap();
        assert!(verify_inclusion(&proof, &leaf(3), &t.root().unwrap()).is_err());
    }

    #[test]
    fn verify_rejects_wrong_sibling() {
        let t = tree_with(8);
        let mut proof = t.prove_inclusion(2).unwrap();
        // Corrupt the first path hop (a mid-path sibling digest).
        proof.sibling_path[0].hash[0] ^= 0xff;
        assert_eq!(
            verify_inclusion(&proof, &leaf(2), &t.root().unwrap()),
            Err(MerkleError::NotMember)
        );
    }

    #[test]
    fn verify_rejects_swapped_sibling_direction() {
        let t = tree_with(8);
        let mut proof = t.prove_inclusion(2).unwrap();
        // Flipping the side of ANY hop changes the reconstruction.
        for s in &mut proof.sibling_path {
            s.left = !s.left;
        }
        assert!(verify_inclusion(&proof, &leaf(2), &t.root().unwrap()).is_err());
    }

    #[test]
    fn verify_rejects_wrong_root() {
        let t = tree_with(8);
        let u = tree_with(9);
        let proof = t.prove_inclusion(2).unwrap();
        let other_root = u.root().unwrap();
        assert_eq!(
            verify_inclusion(&proof, &leaf(2), &other_root),
            Err(MerkleError::RootMismatch {
                proof_root: proof.root_hex(),
                expected_root: hex::encode(other_root)
            })
        );
        assert!(verify_inclusion(&proof, &leaf(3), &t.root().unwrap()).is_err());
    }

    #[test]
    fn prove_out_of_range_and_empty() {
        assert_eq!(IncrementalMerkleTree::new().prove_inclusion(0), None);
        assert_eq!(IncrementalMerkleTree::new().prove_tip(), None);
        let t = tree_with(3);
        assert_eq!(t.prove_inclusion(3), None);
        assert_eq!(t.prove_inclusion(u64::MAX), None);
        let tip = t.prove_tip().unwrap();
        assert_eq!(tip.leaf_index, 2);
        assert!(verify_inclusion(&tip, &leaf(2), &t.root().unwrap()).is_ok());
    }

    #[test]
    fn frontier_stays_bounded_while_streaming() {
        // "bounded memory (streaming)": push a large chain one leaf at a time
        // and assert the live root state (frontier) never exceeds ⌈log2(n+1)⌉
        // peaks, and the incremental root tracks the naive root at every step.
        const N: u64 = 100_003; // deliberately not a power of two
        let mut t = IncrementalMerkleTree::with_chain_id("stream");
        let mut checkpoints = vec![];
        for i in 0..N {
            t.push(leaf(i));
            let bound = (i + 1).ilog2() as usize + 1;
            assert!(
                t.frontier_peaks() <= bound,
                "frontier {} > bound {} at len {}",
                t.frontier_peaks(),
                bound,
                i + 1
            );
            if i % 4096 == 0 {
                checkpoints.push((i + 1, t.root().unwrap()));
            }
        }
        assert_eq!(t.len(), N);
        let final_root = t.root().unwrap();
        for (len, root_at_checkpoint) in checkpoints {
            // Root at a mid-stream checkpoint equals a fresh build of that prefix.
            let prefix: Vec<[u8; 32]> = (0..len).map(leaf).collect();
            assert_eq!(Some(root_at_checkpoint), naive_root(&prefix));
        }
        // And the final root equals a batch build.
        let batch = {
            let mut b = IncrementalMerkleTree::with_chain_id("batch");
            b.extend((0..N).map(leaf));
            b
        };
        assert_eq!(final_root, batch.root().unwrap());
    }

    #[test]
    fn frontier_size_is_logarithmic() {
        let mut t = IncrementalMerkleTree::new();
        for i in 0..1_000_000u64 {
            t.push(leaf(i));
            assert!(t.frontier_peaks() <= (i + 1).ilog2() as usize + 1);
        }
    }

    #[test]
    fn leaf_from_hex_validates() {
        let hex_hash = hex::encode(leaf(7));
        assert_eq!(leaf_from_hex(&hex_hash).unwrap(), leaf(7));
        assert!(matches!(
            leaf_from_hex("abcd"),
            Err(MerkleError::InvalidLeaf(_))
        ));
        assert!(matches!(
            leaf_from_hex("zz"),
            Err(MerkleError::InvalidLeaf(_))
        ));
        assert!(matches!(
            leaf_from_hex(""),
            Err(MerkleError::InvalidLeaf(_))
        ));
    }
}
