//! 稳定身份：内容寻址（材料）与版本链寻址（事件）。
//! 导出 -> 删除本地库 -> 再导入后身份不变。

use crate::domain::canonical::canonical_hex;
use serde::{Deserialize, Serialize};

/// 不可变材料身份：`mat_` + 内容哈希前 16 位。
pub fn material_id(kind: &str, body: &impl serde::Serialize) -> String {
    let tagged = serde_json::json!({"kind": kind, "body": body});
    format!("mat_{}", &canonical_hex(&tagged)[..16])
}

/// 派生计算身份：`calc_` + (规则版本, 输入摘要) 的哈希。
/// 同样输入 + 同样规则版本 => 同样身份（等价输入得到稳定顺序/稳定结果）。
pub fn calc_id(rule_version: &str, input_digest: &str) -> String {
    let tagged = serde_json::json!({"rule_version": rule_version, "input_digest": input_digest});
    format!("calc_{}", &canonical_hex(&tagged)[..16])
}

/// 事件（版本链节点）：`evt_` + 前一事件 + 内容 的哈希。
pub fn event_id(prev: Option<&str>, body: &impl serde::Serialize) -> String {
    let tagged = serde_json::json!({"prev": prev, "body": body});
    format!("evt_{}", &canonical_hex(&tagged)[..20])
}

/// 计划/输出事实身份：`fact_` + 内容哈希。
pub fn fact_id(body: &impl serde::Serialize) -> String {
    format!("fact_{}", &canonical_hex(body)[..20])
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StableKind {
    Material,
    Event,
    Calc,
    Fact,
    SstFile,
    RuleVersion,
}

/// 身份引用：显示值全部携带稳定身份与来源关系。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdRef {
    pub id: String,
    pub kind: StableKind,
    /// 直接来源（父级依赖）的稳定身份列表。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}
