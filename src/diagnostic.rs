//! 带位置的诊断。三类失败在类型上互不相同，绝不以空值/默认值伪装成功。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// 输入片段语法/格式损坏（无法解析）。
    FormatCorrupt,
    /// 资源预算耗尽（字节、候选数等显式预算）。
    BudgetExhausted,
    /// 语法合法、资源足够，但存在多个合法解且规则无法唯一裁决。
    SemanticAmbiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
    /// 源材料稳定身份（哪条材料）。
    pub source_id: String,
    /// 命令序号/偏移（便于在片段内定位）。
    pub command_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub class: FailureClass,
    pub code: String,
    pub message: String,
    pub pos: Option<Pos>,
    /// 无法转换而保留在原位置的值（例如不可解析尺寸）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub retained_raw: Option<String>,
}

impl Diagnostic {
    pub fn corrupt(code: impl Into<String>, msg: impl Into<String>, pos: Option<Pos>) -> Self {
        Diagnostic { class: FailureClass::FormatCorrupt, code: code.into(), message: msg.into(), pos, retained_raw: None }
    }
    pub fn budget(code: impl Into<String>, msg: impl Into<String>, pos: Option<Pos>) -> Self {
        Diagnostic { class: FailureClass::BudgetExhausted, code: code.into(), message: msg.into(), pos, retained_raw: None }
    }
    pub fn ambiguous(code: impl Into<String>, msg: impl Into<String>, pos: Option<Pos>) -> Self {
        Diagnostic { class: FailureClass::SemanticAmbiguous, code: code.into(), message: msg.into(), pos, retained_raw: None }
    }
    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.retained_raw = Some(raw.into());
        self
    }
}

pub type DiagResult<T> = Result<T, Diagnostic>;
