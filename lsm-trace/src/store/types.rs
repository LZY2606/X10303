//! 存储层行类型与事件载荷类型。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id: String,
    pub branch_id: String,
    pub seq: i64,
    pub parent_id: Option<String>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub payload_digest: String,
    pub occurred_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchRow {
    pub id: String,
    pub case_id: String,
    pub name: String,
    pub parent_event_id: Option<String>,
    pub head_event_id: Option<String>,
    pub created_seq: i64,
    pub is_main: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRow {
    pub id: String,
    pub level: i64,
    pub origin: String,
    pub smallest: Option<String>,
    pub largest: Option<String>,
    pub size_bytes: Option<i64>,
    pub size_unit: Option<String>,
    pub born_event_id: String,
    pub dead_event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryRow {
    pub id: String,
    pub key: String,
    pub seq: i64,
    pub kind: i64,
    pub value: Option<String>,
    pub value_unit: Option<String>,
    pub batch_id: Option<String>,
    pub file_id: Option<String>,
    pub born_event_id: String,
    pub dropped_event_id: Option<String>,
    pub dropped_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RangeRow {
    pub id: String,
    pub seq: i64,
    pub start: String,
    pub end: String,
    pub file_id: Option<String>,
    pub born_event_id: String,
    pub dropped_event_id: Option<String>,
    pub dropped_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRow {
    pub id: String,
    pub last_seq: i64,
    pub seq_unit: String,
    pub state: String,
    pub born_event_id: String,
    pub dead_event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagRow {
    pub event_id: Option<String>,
    pub raw_ref: Option<String>,
    pub code: String,
    pub severity: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RulesetRow {
    pub id: String,
    pub version: i64,
    pub event_id: String,
    pub body: serde_json::Value,
    pub note: String,
}
