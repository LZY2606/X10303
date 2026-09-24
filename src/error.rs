//! 三类互不等价的失败结果：
//! - [`AppError::ResourceExhausted`] 资源预算耗尽（可重试运行更大预算）
//! - [`AppError::FormatCorrupt`]     格式损坏（数据/包不合法，带位置）
//! - [`AppError::SemanticUnsolvable`] 语义无解（数据合法但无法唯一/合法决定，带全部候选）
//!
//! 它们是三个不同枚举变体，序列化 kind 不同，绝不相互伪装。

use serde::Serialize;
use std::fmt;

/// 带位置的诊断（输入不完整/无法转换时返回，不用空值或默认值伪装成功）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub location: String,
    pub message: String,
}

impl Diagnostic {
    pub fn new(code: impl Into<String>, location: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), location: location.into(), message: message.into() }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.code, self.location, self.message)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppError {
    /// 资源预算耗尽：如输出字节数超过 max_output_bytes、候选数超过上限。
    ResourceExhausted {
        budget: String,
        used: String,
        limit: String,
        detail: String,
    },
    /// 格式损坏：文件/输入/导出包结构非法或内容指纹不符。
    FormatCorrupt { diagnostics: Vec<Diagnostic> },
    /// 语义无解：存在矛盾、重叠约束被破坏，或无法唯一决定且未给出选择。
    SemanticUnsolvable {
        reason: String,
        diagnostics: Vec<Diagnostic>,
        candidates: Vec<serde_json::Value>,
    },
    /// 档案一致性错误（身份冲突、父依赖不存在等）。
    Conflict { message: String },
    NotFound { what: String, id: String },
}

impl AppError {
    pub fn corrupt(d: Diagnostic) -> Self {
        AppError::FormatCorrupt { diagnostics: vec![d] }
    }
    pub fn corrupt_many(ds: Vec<Diagnostic>) -> Self {
        AppError::FormatCorrupt { diagnostics: ds }
    }
    pub fn unsolvable(reason: impl Into<String>, d: Diagnostic, candidates: Vec<serde_json::Value>) -> Self {
        AppError::SemanticUnsolvable { reason: reason.into(), diagnostics: vec![d], candidates }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::ResourceExhausted { detail, .. } => write!(f, "resource exhausted: {detail}"),
            AppError::FormatCorrupt { diagnostics } => {
                write!(f, "format corrupt: ")?;
                for d in diagnostics {
                    write!(f, "{d}; ")?
                }
                Ok(())
            }
            AppError::SemanticUnsolvable { reason, .. } => write!(f, "semantic unsolvable: {reason}"),
            AppError::Conflict { message } => write!(f, "conflict: {message}"),
            AppError::NotFound { what, id } => write!(f, "not found: {what} {id}"),
        }
    }
}

impl std::error::Error for AppError {}

pub type AppResult<T> = Result<T, AppError>;
