use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

/// Logical sequence number (monotonic logical clock; unit: seq).
pub type Seq = u64;

/// Value type of an internal record. Ordering is load-bearing:
/// the internal-key comparator orders kinds descending, so at equal
/// sequence numbers a Put sorts before a Delete deterministically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ValueType {
    Delete = 0,
    Put = 1,
}

impl ValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::Delete => "delete",
            ValueType::Put => "put",
        }
    }
}

/// Internal key = (user_key, seq, kind). Coordinate basis for user keys is
/// lexicographic UTF-8 byte order; seq is a monotonic logical clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalKey {
    pub user_key: String,
    pub seq: Seq,
    pub kind: ValueType,
}

impl InternalKey {
    pub fn new(user_key: &str, seq: Seq, kind: ValueType) -> Self {
        Self { user_key: user_key.to_string(), seq, kind }
    }

    /// RocksDB-style internal ordering: user_key asc, seq desc, kind desc.
    pub fn cmp_internal(a: &InternalKey, b: &InternalKey) -> Ordering {
        a.user_key
            .cmp(&b.user_key)
            .then_with(|| b.seq.cmp(&a.seq))
            .then_with(|| b.kind.cmp(&a.kind))
    }

    pub fn label(&self) -> String {
        format!("{}#{}:{}", self.user_key, self.seq, self.kind.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointEntry {
    pub key: InternalKey,
    pub value: Option<String>,
}

impl PointEntry {
    pub fn put(user_key: &str, seq: Seq, value: &str) -> Self {
        Self { key: InternalKey::new(user_key, seq, ValueType::Put), value: Some(value.to_string()) }
    }
    pub fn delete(user_key: &str, seq: Seq) -> Self {
        Self { key: InternalKey::new(user_key, seq, ValueType::Delete), value: None }
    }
    pub fn label(&self) -> String {
        self.key.label()
    }
}

/// Range tombstone covering the half-open interval [start, end).
/// A point with sequence `s` is covered iff start <= key < end AND s < seq.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RangeDel {
    pub start: String,
    pub end: String,
    pub seq: Seq,
}

impl RangeDel {
    pub fn covers_key(&self, user_key: &str) -> bool {
        self.start.as_str() <= user_key && user_key < self.end.as_str()
    }
    pub fn covers(&self, user_key: &str, seq: Seq) -> bool {
        self.covers_key(user_key) && seq < self.seq
    }
    pub fn overlaps_user_range(&self, lo: &str, hi: &str) -> bool {
        self.start.as_str() < hi && lo < self.end.as_str()
    }
    pub fn label(&self) -> String {
        format!("[{},{})#{}", self.start, self.end, self.seq)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SsTable {
    pub id: String,
    pub level: u32,
    pub points: Vec<PointEntry>,
    pub range_dels: Vec<RangeDel>,
    pub smallest: InternalKey,
    pub largest: InternalKey,
    pub byte_size: usize,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub seq: Seq,
}

/// Immutable, append-only review event. The payload records the exact
/// conclusions seen at that time, so revisiting an old event restores the
/// historical visibility verdict instead of recomputing with latest state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub ord: u64,
    pub kind: String,
    pub summary: String,
    pub payload: Value,
    pub parents: Vec<String>,
    pub param_version: u64,
    pub input_digest: String,
}

/// Mutable decision layer, kept separate from immutable materials/events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub ord: u64,
    pub target: String,
    pub kind: String, // accept | reject | param_adjust
    pub note: String,
    pub supersedes: Option<String>,
    pub param_version: u64,
}

/// Canonical SHA-256 digest over serde JSON (maps serialize with sorted keys).
pub fn digest_json<T: Serialize>(value: &T) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_default();
    let hash = Sha256::digest(encoded.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unit / coordinate-basis legend shown by the review UI and embedded in
/// every baseline payload.
pub fn units_json() -> Value {
    serde_json::json!({
        "user_key_basis": "lexicographic UTF-8 byte order; key ranges are half-open coordinates on that basis",
        "seq_basis": "sequence number: monotonic logical clock (unitless integer)",
        "size_unit": "bytes",
        "level_basis": "L0 files may overlap; L1+ files must be key-disjoint and ordered",
        "range_basis": "range tombstones cover the half-open interval [start, end)",
        "time_basis": "event ord: monotonic append order",
        "visibility_basis": "internal-key order (user asc, seq desc, kind desc); entry visible at snapshot s iff seq<=s and not covered by a range tombstone with seq<=s"
    })
}
