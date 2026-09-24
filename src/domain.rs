use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

pub const KEY_BASIS: &str = "user-key/utf8-byte-order";
pub const HALF_OPEN_BASIS: &str = "user-key/half-open-[start,end)";
pub const SEQ_UNIT: &str = "sequence-number";
pub const LEVEL_UNIT: &str = "lsm-level";
pub const BYTE_UNIT: &str = "bytes";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Put,
    PointDelete,
    RangeDelete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SameSeqPolicy {
    DeleteBeforeValue,
    ValueBeforeDelete,
    RequireDistinctSequence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seq {
    pub raw: String,
    pub value: u64,
    #[serde(rename = "unit")]
    pub unit: String,
}

impl Seq {
    pub fn new(value: u64) -> Self {
        Self {
            raw: value.to_string(),
            value,
            unit: SEQ_UNIT.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coord {
    pub raw: String,
    pub normalized: String,
    pub coordinate_basis: String,
}

impl Coord {
    pub fn key(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        Self {
            normalized: raw.clone(),
            raw,
            coordinate_basis: KEY_BASIS.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quantity {
    pub raw: String,
    pub normalized: Option<String>,
    pub unit: String,
    pub basis: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalEntry {
    pub id: String,
    pub user_key: Coord,
    pub seq: Seq,
    pub kind: EntryKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<Quantity>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub range_end: Option<Coord>,
    #[serde(default)]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SstSummary {
    pub id: String,
    pub level_raw: String,
    pub level: u8,
    pub level_unit: String,
    pub start: Coord,
    pub end: Coord,
    pub range_basis: String,
    pub entry_ids: Vec<String>,
    pub size: Quantity,
    pub overlaps: bool,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: Seq,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleParams {
    pub version: String,
    pub same_seq_policy: SameSeqPolicy,
    pub max_output_files: usize,
    pub max_entries_per_output: usize,
    pub require_all_overlapping_l0: bool,
    pub range_basis: String,
}

impl Default for RuleParams {
    fn default() -> Self {
        Self {
            version: "rules-v1".to_string(),
            same_seq_policy: SameSeqPolicy::DeleteBeforeValue,
            max_output_files: 4,
            max_entries_per_output: 3,
            require_all_overlapping_l0: true,
            range_basis: HALF_OPEN_BASIS.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchState {
    pub branch: String,
    pub rules: RuleParams,
    pub entries: Vec<InternalEntry>,
    pub ssts: Vec<SstSummary>,
    pub snapshots: Vec<Snapshot>,
    #[serde(default)]
    pub corrupt_fragments: Vec<CorruptFragment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorruptFragment {
    pub id: String,
    pub raw: String,
    pub location: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub location: String,
}

impl Diagnostic {
    pub fn new(code: impl Into<String>, location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            location: location.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComputeStatus {
    Success,
    ResourceExhausted,
    FormatCorrupt,
    SemanticNoSolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Judgment {
    pub code: String,
    pub subject_id: String,
    pub decision: String,
    pub reason: String,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSst {
    pub id: String,
    pub level: u8,
    pub start: String,
    pub end: String,
    pub range_basis: String,
    pub entry_ids: Vec<String>,
    pub boundary_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadVisibility {
    pub snapshot_id: String,
    pub user_key: String,
    pub visible_entry_id: Option<String>,
    pub visible: bool,
    pub reason_code: String,
    pub chain_ids: Vec<String>,
    pub range_shield_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSummary {
    pub state_digest: String,
    pub selected_sst_ids: Vec<String>,
    pub entry_ids: Vec<String>,
    pub snapshot_ids: Vec<String>,
    pub rule_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionResult {
    pub id: String,
    pub scenario: String,
    pub branch: String,
    pub status: ComputeStatus,
    pub input_summary: InputSummary,
    #[serde(default)]
    pub kept_entry_ids: Vec<String>,
    #[serde(default)]
    pub dropped_entry_ids: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<OutputSst>,
    #[serde(default)]
    pub visibility: Vec<ReadVisibility>,
    #[serde(default)]
    pub judgments: Vec<Judgment>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub legal_candidates: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Kept,
    Dropped,
}

pub fn canonical_digest<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).expect("serializable input");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:.16x}", hasher.finalize())
}

pub fn same_seq_kind_rank(kind: EntryKind, policy: SameSeqPolicy) -> u8 {
    match (kind, policy) {
        (EntryKind::PointDelete, SameSeqPolicy::DeleteBeforeValue)
        | (EntryKind::RangeDelete, SameSeqPolicy::DeleteBeforeValue)
        | (EntryKind::Put, SameSeqPolicy::ValueBeforeDelete) => 0,
        (EntryKind::Put, SameSeqPolicy::DeleteBeforeValue)
        | (EntryKind::PointDelete, SameSeqPolicy::ValueBeforeDelete)
        | (EntryKind::RangeDelete, SameSeqPolicy::ValueBeforeDelete) => 1,
        (_, SameSeqPolicy::RequireDistinctSequence) => 2,
    }
}

pub fn internal_cmp(left: &InternalEntry, right: &InternalEntry, policy: SameSeqPolicy) -> Ordering {
    left.user_key
        .normalized
        .cmp(&right.user_key.normalized)
        .then_with(|| right.seq.value.cmp(&left.seq.value))
        .then_with(|| {
            same_seq_kind_rank(left.kind, policy).cmp(&same_seq_kind_rank(right.kind, policy))
        })
        .then_with(|| left.id.cmp(&right.id))
}
