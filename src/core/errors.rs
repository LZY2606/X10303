//! 三类必须可区分的失败结果：
//! - [`Error::BudgetExhausted`]：资源预算耗尽（输出切分超出预算、磁盘/批次预算等）。
//! - [`Error::Corrupt`]：格式损坏（校验和、帧结构、缺字段、导出包哈希不符等）。
//! - [`Error::SemanticUnresolved`]：语义无解（多个合法候选无法唯一决定，且策略要求拒绝）。

use crate::core::model::Diagnostic;

#[derive(Debug)]
pub enum Error {
    /// 资源预算耗尽。resource 指明哪类预算；limit/used 带单位。
    BudgetExhausted {
        resource: String,
        limit: String,
        used: String,
        unit: String,
        message: String,
    },
    /// 格式损坏：附带来自导入/重放位置的诊断。
    Corrupt { diagnostics: Vec<Diagnostic>, message: String },
    /// 语义无解：保留全部合法候选，无法唯一决定。
    SemanticUnresolved {
        candidates: Vec<String>,
        basis: String,
        message: String,
    },
    /// 事件/分支一致性问题（不属于上述三类领域失败，但必须显式失败）。
    Conflict(String),
    NotFound(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BudgetExhausted { message, .. } => write!(f, "budget exhausted: {message}"),
            Error::Corrupt { message, .. } => write!(f, "corrupt: {message}"),
            Error::SemanticUnresolved { message, .. } => {
                write!(f, "semantic unresolved: {message}")
            }
            Error::Conflict(m) => write!(f, "conflict: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// 稳定的机器可读错误分类代码。
    pub fn code(&self) -> &'static str {
        match self {
            Error::BudgetExhausted { .. } => "budget_exhausted",
            Error::Corrupt { .. } => "corrupt",
            Error::SemanticUnresolved { .. } => "semantic_unresolved",
            Error::Conflict(_) => "conflict",
            Error::NotFound(_) => "not_found",
        }
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        match self {
            Error::Corrupt { diagnostics, .. } => diagnostics.clone(),
            _ => Vec::new(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
