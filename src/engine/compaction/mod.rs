//! 压实（compaction）：选择输入、按内部键合并、依 snapshot 与“未参与压实的旧数据”
//! 决定 point/range tombstone 的保留与下沉、输出切分维持不重叠层约束。
//!
//! 三种失败显式区分：资源预算耗尽 / 格式损坏 / 语义无解。

mod split;

pub use split::{candidate_cuts, SplitPlan};

use crate::engine::budget::{check_sst_sizes_usable, Budget};
use crate::engine::error::{AppError, Diagnostic, Pos};
use crate::engine::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 一条记录在压实中的去留判定（机器可读证据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropReason {
    pub point_id: String,
    pub user_key: String,
    pub seqno: i64,
    pub kind: RecordKind,
    /// drop | retain | conflict
    pub disposition: String,
    /// 原因代码
    pub reason: String,
    /// 关联对象（阻断下沉的旧 SSTable、冲突对、掩蔽 range 等）
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeDecision {
    pub range_id: String,
    pub fragment_index: i32,
    pub start_logical: String,
    pub end_logical: String,
    pub end_inclusive: bool,
    pub seqno: i64,
    /// retain | drop
    pub disposition: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<String>,
}

/// 压实选择理由（为什么选这些输入）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionReason {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<String>,
}

/// 关键判断依据（机器可读事件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEvent {
    /// 稳定代码
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSstSpec {
    pub index: usize,
    pub first_user_key: String,
    pub last_user_key: String,
    pub point_ids: Vec<String>,
    pub range_fragments: Vec<String>,
    pub point_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionReport {
    pub request_id: String,
    pub params_version: i64,
    pub params_digest: String,
    pub input_sst_ids: Vec<String>,
    pub input_digest: String,
    pub output_level: i32,
    pub oldest_active_snapshot: Option<i64>,
    pub selection: Vec<SelectionReason>,
    pub point_decisions: Vec<DropReason>,
    pub range_decisions: Vec<RangeDecision>,
    pub surviving_point_ids: Vec<String>,
    /// 采纳的切分（按用户键边界的连续段）
    pub adopted_split: SplitPlan,
    /// 无法唯一决定时保留的全部合法候选切分（切分点）
    pub candidate_splits: Vec<Vec<String>>,
    pub outputs: Vec<OutputSstSpec>,
    pub evidence: Vec<EvidenceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CompactionOutcome {
    /// 压实成功，输出报告
    Report { report: CompactionReport },
    /// 资源预算耗尽
    ResourceExhausted { code: String, message: String, req_id: String },
    /// 格式损坏
    Corrupt { code: String, message: String, req_id: String },
    /// 语义无解，保留全部合法候选（如切分无法满足非重叠层 / 同序号多类型不裁决）
    SemanticUnsolvable {
        code: String,
        message: String,
        req_id: String,
        candidates: Vec<String>,
        evidence: Vec<EvidenceEvent>,
    },
}

fn diag(path: &str, id: &str, code: &str, message: String) -> AppError {
    AppError::Corrupt {
        diagnostic: Diagnostic {
            code: code.to_string(),
            message,
            pos: Pos { path: path.to_string(), object_id: Some(id.to_string()) },
        },
    }
}

fn req_path(req: &str, tail: &str) -> String {
    format!("compaction[{req}].{tail}")
}
