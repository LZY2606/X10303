//! 三种不同结果必须显式区分：
//! - 资源预算耗尽 ResourceExhausted
//! - 格式损坏 Corrupt
//! - 语义无解 SemanticUnsolvable
//! 输入不完整返回带位置的诊断，绝不用空值/默认值伪装成功。

use serde::{Deserialize, Serialize};

/// 诊断位置（材料路径 + 稳定身份 + 字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    /// 材料在导入载荷中的路径，如 "sstables[3].points[1].value"
    pub path: String,
    /// 涉及对象的稳定身份（若已有）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub object_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub pos: Pos,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {} @ {}", self.code, self.message, self.pos.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppError {
    /// 输入不完整 / 结构缺失：带位置诊断
    Incomplete { diagnostics: Vec<Diagnostic> },
    /// 格式损坏（编码、范围、JSON 结构无法解释）
    Corrupt { diagnostic: Diagnostic },
    /// 资源预算耗尽（输出条数/字节/候选数超预算）
    ResourceExhausted { diagnostic: Diagnostic },
    /// 语义无解（同序号多类型无法唯一裁决、非重叠层无法满足等）
    SemanticUnsolvable { diagnostic: Diagnostic, candidates: Vec<String> },
    /// 存储层错误
    Storage { message: String },
    /// 请求冲突（如重复安装但内容不一致）
    Conflict { message: String },
}

impl AppError {
    pub fn incomplete(path: impl Into<String>, code: &str, msg: impl Into<String>) -> Self {
        AppError::Incomplete {
            diagnostics: vec![Diagnostic {
                code: code.to_string(),
                message: msg.into(),
                pos: Pos { path: path.into(), object_id: None },
            }],
        }
    }
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Incomplete { .. } => "INCOMPLETE",
            AppError::Corrupt { .. } => "CORRUPT",
            AppError::ResourceExhausted { .. } => "RESOURCE_EXHAUSTED",
            AppError::SemanticUnsolvable { .. } => "SEMANTIC_UNSOLVABLE",
            AppError::Storage { .. } => "STORAGE",
            AppError::Conflict { .. } => "CONFLICT",
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Incomplete { diagnostics } => {
                write!(f, "INCOMPLETE: ")?;
                for d in diagnostics {
                    write!(f, "{d}; ")?;
                }
                Ok(())
            }
            AppError::Corrupt { diagnostic } => write!(f, "CORRUPT: {diagnostic}"),
            AppError::ResourceExhausted { diagnostic } => write!(f, "RESOURCE_EXHAUSTED: {diagnostic}"),
            AppError::SemanticUnsolvable { diagnostic, candidates } => {
                write!(f, "SEMANTIC_UNSOLVABLE: {diagnostic}; candidates={candidates:?}")
            }
            AppError::Storage { message } => write!(f, "STORAGE: {message}"),
            AppError::Conflict { message } => write!(f, "CONFLICT: {message}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Storage { message: e.to_string() }
    }
}
