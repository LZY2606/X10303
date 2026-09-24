//! 机器可读取的事件。
//!
//! 事件是追加式的不可变事实；派生状态完全由事件回放得到。
//! 每条事件都携带: 稳定身份、父事件、分支、输入摘要、规则版本、证据。

use crate::domain::model::{Json, RuleSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    ProjectCreated,
    MaterialsImported,
    MaterialSuperseded,
    RuleRevised,
    CompactionPrepared,
    CompactionInterrupted,
    CompactionInstalled,
    DuplicateInstallRejected,
    CompactionDecision,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionVerdict {
    Accepted,
    Rejected,
    ParameterAdjustment,
    Conflict,
}

/// 压实请求 (可在准备阶段显式给出输入, 也可由种子/层级自动选择)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionRequest {
    pub scenario: String,
    pub target_level: u8,
    /// 显式输入 SSTable id; 空则依据 seed_levels 自动选择。
    #[serde(default)]
    pub input_ssts: Vec<String>,
    #[serde(default)]
    pub seed_levels: Vec<u8>,
    /// 模拟输出切分边界 (user key)。
    #[serde(default)]
    pub output_split_keys: Vec<String>,
    /// 资源预算 (输出条目上限); 耗尽是与损坏/语义无解不同的结果。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_entry_budget: Option<usize>,
    /// 准备后中断, 不进入安装。
    #[serde(default)]
    pub interrupt_after_prepare: bool,
    /// 人工对输出候选的选择 (冲突决定)。
    #[serde(default)]
    pub manual_candidate_pick: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub seq_no: u64,
    pub branch: String,
    pub kind: EventKind,
    pub created_at: String,
    /// 父事件 id (同一分支上的前一事件)。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_id: Option<String>,
    /// 分支起点: 从哪条 (主) 事件分叉。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fork_from_event: Option<String>,
    /// 输入摘要: 被读取的材料/状态对象 id 与内容哈希。
    #[serde(default)]
    pub input_summary: Json,
    /// 当时生效的规则版本。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rule_version: Option<String>,
    /// 决定裁决 (接受/否定/参数调整/冲突)。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub verdict: Option<DecisionVerdict>,
    /// 事件载荷 (按 kind 解释)。
    pub payload: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectCreated {
    pub project_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterialsImported {
    pub pack_id: String,
    pub pack_version: String,
    pub source: String,
    pub material_ids: Vec<String>,
    pub materials_hash: String,
    /// 导入诊断 (无法转换的值留原位置并附诊断, 但不伪装成功之外的语义)。
    #[serde(default)]
    pub diagnostics: Vec<crate::domain::model::Diagnostic>,
    /// 是否为半写恢复后的登记。
    #[serde(default)]
    pub recovered_from_interrupted_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterialSuperseded {
    pub old_material_id: String,
    pub new_material_id: String,
    pub material_kind: String,
    pub reason: String,
    /// 受影响的派生对象 (历史事件中的结论不被覆盖, 只标记为失配)。
    #[serde(default)]
    pub affected_derivations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleRevised {
    pub rules: RuleSet,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionDecisionBody {
    pub run_id: String,
    pub decision: DecisionVerdict,
    pub note: String,
    /// 参数调整时携带新规则版本; 冲突决定时携带冲突描述。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rule_version: Option<String>,
}

/// 导出包: 材料 + 事件 (含分支与决定历史) 身份保持。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectExport {
    pub format: String,
    pub project_id: String,
    pub exported_at: String,
    pub branches: Vec<BranchInfo>,
    pub events: Vec<Event>,
    pub current_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub head_event_id: Option<String>,
    pub created_at: String,
}
