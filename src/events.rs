//! 机器可读取的审计事件。事件不可变；身份 = 内容(含父事件 id)的哈希。
//! 分支是独立的线性事件链；回到任意旧事件即可重放当时的世界状态。

use crate::model::{CompactionParams, Container, Diagnostic, SnapshotSpec};
use serde::{Deserialize, Serialize};

pub const BRANCH_MAIN: &str = "br_main";

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 初始导入（基线材料，含原始表示）。
    MaterialImported,
    /// 参数修订（如收紧切分阈值 / 改变墓碑丢弃策略）。
    ParameterRevision,
    /// 只影响局部依赖的源数据改动（替换单个容器，记录影响范围）。
    SourceChange,
    /// 人工决定：接受 / 否定 / 参数调整。冲突的人工决定保留全部候选。
    Decision,
    /// 压实计划（选择理由、候选、判定状态；尚未安装）。
    CompactionPlanned,
    /// 压实安装结果（输出、被删输入、步骤状态；重复安装幂等）。
    CompactionInstalled,
    /// 压实中断 / 半写恢复。
    CompactionInterrupted,
    /// 快照存活状态切换（旧 snapshot 被释放 / 重新持有）。
    SnapshotToggle,
    /// 场景重放标记（一段规范事件序列的回放）。
    ScenarioReplayed,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ImportPayload {
    pub case_id: String,
    pub case_version: String,
    pub title: String,
    pub raw_source_fingerprint: String,
    pub containers: Vec<Container>,
    pub snapshots: Vec<SnapshotSpec>,
    pub params: CompactionParams,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ParamsRevisionPayload {
    pub from_version: String,
    pub new_params: CompactionParams,
    pub rationale: String,
    /// 受影响的下游（计划/结果 id）。
    pub affects: Vec<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeScope {
    /// 只影响引用该容器的局部依赖。
    LocalDependency,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct SourceChangePayload {
    pub replaces_container_id: String,
    pub new_container: Container,
    pub scope: ChangeScope,
    pub rationale: String,
    /// 被失效的派生结果（计划 id 等），可从最终状态反查差异。
    pub invalidates: Vec<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accept,
    Reject,
    Adjust,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct DecisionPayload {
    pub target_plan_id: String,
    pub verdict: Verdict,
    pub note: String,
    /// 与既有决定冲突时，保留全部合法候选；此处记录冲突对象。
    #[serde(default)]
    pub conflicts_with: Vec<String>,
    #[serde(default)]
    pub proposed_params: Option<CompactionParams>,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct SnapshotTogglePayload {
    pub snapshot_id: String,
    pub alive: bool,
    pub reason: String,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ScenarioPayload {
    pub scenario_id: String,
    pub title: String,
    /// 该场景实际经过的边界情形标签。
    pub traversed: Vec<String>,
    pub expanded_events: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Event {
    pub id: String,
    pub branch: String,
    pub ord: i64,
    pub kind: EventKind,
    pub title: String,
    pub parent_id: Option<String>,
    pub content_hash: String,
    pub created_at_ms: i64,
    pub payload: serde_json::Value,
}
