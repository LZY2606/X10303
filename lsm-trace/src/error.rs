//! 三类必须区分的失败结果，以及带位置的输入诊断。
//!
//! - [`AppError::BudgetExhausted`]：资源预算（输出文件数/暂存行数等）耗尽
//! - [`AppError::Corrupt`]：格式损坏（无法解释的字节、引用断裂、校验不符）
//! - [`AppError::Unsolvable`]：输入完整且合法，但语义上没有唯一/合法解
//! - [`AppError::Diagnostics`]：输入不完整或不满足 schema，诊断带 JSON Pointer 位置
//!
//! 任何一个分支都不得用空值或默认值伪装成功。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Diag {
    pub code: String,
    pub severity: String,
    /// 指向输入片段的 JSON Pointer，例如 `/batches/2/records/0/seq`。
    pub path: String,
    pub message: String,
    /// 造成诊断的原始记录身份（若可定位）。
    pub raw_ref: Option<String>,
}

impl Diag {
    pub fn err(code: impl Into<String>, path: impl Into<String>, msg: impl Into<String>) -> Self {
        Diag { code: code.into(), severity: "error".into(), path: path.into(), message: msg.into(), raw_ref: None }
    }
    pub fn warn(code: impl Into<String>, path: impl Into<String>, msg: impl Into<String>) -> Self {
        Diag { code: code.into(), severity: "warning".into(), path: path.into(), message: msg.into(), raw_ref: None }
    }
    pub fn at(mut self, raw_ref: impl Into<String>) -> Self {
        self.raw_ref = Some(raw_ref.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetInfo {
    pub budget: String,
    pub limit: u64,
    pub used: u64,
    pub needed: u64,
    pub context: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppError {
    /// 输入不完整 / 不满足 schema：带位置的诊断集合。
    Diagnostics { diagnostics: Vec<Diag> },
    /// 资源预算耗尽——输入本身没有问题。
    BudgetExhausted(BudgetInfo),
    /// 格式损坏——无法可信解释数据。
    Corrupt { diagnostics: Vec<Diag> },
    /// 语义无解——存在互相冲突的硬约束，没有合法候选。
    Unsolvable { diagnostics: Vec<Diag> },
    NotFound { what: String },
    Conflict { message: String },
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}
impl std::error::Error for AppError {}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Diagnostics { .. } => "diagnostics",
            AppError::BudgetExhausted(_) => "budget_exhausted",
            AppError::Corrupt { .. } => "corrupt",
            AppError::Unsolvable { .. } => "unsolvable",
            AppError::NotFound { .. } => "not_found",
            AppError::Conflict { .. } => "conflict",
        }
    }

    /// 诊断类错误（输入不完整/非法）。
    pub fn diagnostics(ds: Vec<Diag>) -> Self {
        AppError::Diagnostics { diagnostics: ds }
    }
    pub fn corrupt(ds: Vec<Diag>) -> Self {
        AppError::Corrupt { diagnostics: ds }
    }
    pub fn unsolvable(ds: Vec<Diag>) -> Self {
        AppError::Unsolvable { diagnostics: ds }
    }
}

/// 在收集诊断时使用：既记录 error 级诊断，也允许继续扫描更多位置。
#[derive(Default, Debug)]
pub struct DiagSink {
    pub items: Vec<Diag>,
}

impl DiagSink {
    pub fn new() -> Self { Self::default() }
    pub fn push(&mut self, d: Diag) { self.items.push(d); }
    pub fn error(&mut self, code: &str, path: &str, msg: impl Into<String>) {
        self.items.push(Diag::err(code, path, msg));
    }
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == "error")
    }
}
