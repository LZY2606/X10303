//! 三类必须可区分的结果：格式损坏 / 资源预算耗尽 / 语义无解。
//!
//! 三者都携带可定位信息（位置、预算、候选），调用方（重放、导入、API）
//! 必须按类型分别处理，不能合并成一个通用错误。

use serde::{Deserialize, Serialize};

/// 带位置的诊断，输入不完整或无法转换时使用；绝不允许用空值/默认值伪装成功。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub severity: DiagSeverity,
    pub code: String,
    /// 例如 `materials/sstables.json#/files/2/records/0/key_raw`
    pub location: String,
    pub message: String,
    /// 触发诊断的原始片段（保留原始表示，不丢弃）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DomainError {
    /// 结构/编码损坏：无法解析、哈希链断裂、字节解码失败等。
    FormatCorrupt {
        location: String,
        reason: String,
        raw: String,
    },
    /// 资源预算耗尽（输出字节、步骤数等），区别于语义问题。
    ResourceExhausted {
        budget: String,
        limit: u64,
        used: u64,
        at: String,
    },
    /// 语义无解：存在多个合法候选且没有唯一结论，必须保留全部候选。
    SemanticUnsolvable {
        location: String,
        reason: String,
        candidates: Vec<serde_json::Value>,
    },
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DomainError::FormatCorrupt { location, reason, .. } => {
                write!(f, "格式损坏 @ {location}: {reason}")
            }
            DomainError::ResourceExhausted { budget, limit, used, at } => {
                write!(f, "预算耗尽[{budget}] used={used} limit={limit} @ {at}")
            }
            DomainError::SemanticUnsolvable { location, reason, .. } => {
                write!(f, "语义无解 @ {location}: {reason}")
            }
        }
    }
}

impl std::error::Error for DomainError {}

pub type DResult<T> = Result<T, DomainError>;
