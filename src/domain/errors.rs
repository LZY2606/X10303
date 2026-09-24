//! 三类互不等价的失败结果：资源预算耗尽 / 格式损坏 / 语义无解。
//! 测试直接按判别式区分，绝不允许折叠成同一种错误。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticPos {
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum LoadError {
    /// 资源预算耗尽：算法本身有合法解，但当前 budget 放不下。
    BudgetExhausted {
        job: String,
        needed_entries: u32,
        capacity_entries: u32,
        needed_outputs: u32,
        max_outputs: u32,
        pos: DiagnosticPos,
    },
    /// 格式损坏：材料自相矛盾/缺失，输入不完整。返回带位置的诊断，禁止用空值或默认值伪装成功。
    Corrupt { pos: DiagnosticPos, diagnostics: Vec<String> },
    /// 语义无解：存在多于一个合法解释，保留全部合法候选。
    SemanticUnresolved {
        pos: DiagnosticPos,
        candidates: Vec<serde_json::Value>,
        diagnostics: Vec<String>,
    },
}

impl LoadError {
    pub fn class(&self) -> &'static str {
        match self {
            LoadError::BudgetExhausted { .. } => "budget_exhausted",
            LoadError::Corrupt { .. } => "corrupt",
            LoadError::SemanticUnresolved { .. } => "semantic_unresolved",
        }
    }

    pub fn pos(&self) -> &DiagnosticPos {
        match self {
            LoadError::BudgetExhausted { pos, .. } => pos,
            LoadError::Corrupt { pos, .. } => pos,
            LoadError::SemanticUnresolved { pos, .. } => pos,
        }
    }
}

pub type LoadResult<T> = Result<T, LoadError>;

/// 输入不完整：位置化诊断的快捷构造。
pub fn corrupt(path: impl Into<String>, detail: impl Into<String>) -> LoadError {
    LoadError::Corrupt {
        pos: DiagnosticPos { path: path.into(), detail: detail.into() },
        diagnostics: Vec::new(),
    }
}
