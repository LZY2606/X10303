//! 事件（event sourcing）。任何状态变化或审阅结论都落为不可变事件；
//! 派生状态完全由按序重放事件得到，回到任意旧事件即恢复当时依赖版本与结论。

use crate::core::model::{CompactionParams, Diagnostic, Material, Snapshot};
use serde::{Deserialize, Serialize};

/// 稳定事件身份：全局单调递增 id（同分支序列），附父事件与前一条事件以校验顺序。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 稳定身份；导出再导入保持。
    pub id: u64,
    pub branch: String,
    /// 分支内直接父事件（fork 点或上一事件）。
    pub parent_id: u64,
    /// 全局前一条事件，用于断言“等价输入得到稳定顺序”。
    pub prev_id: Option<u64>,
    pub kind: EventKind,
    pub payload: EventPayload,
    /// 依赖版本快照：该事件计算时采用的规则版本与参数版本。
    pub rule_version: String,
    pub params_version: u32,
    /// 输入摘要（内容哈希列表）。
    pub input_digests: Vec<String>,
    /// 直接来源：由哪条材料 / 事件 / 决定触发。
    pub direct_source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    ProjectCreated,
    MaterialImported,
    ImportDiagnostic,
    SnapshotCreated,
    SnapshotReleased,
    ParamsSet,
    FlushInstalled,
    CompactionPlanned,
    CompactionDecision,
    CompactionInstalled,
    CompactionInterrupted,
    RecoveryApplied,
    ReadAudit,
    BranchForked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    ProjectCreated { name: String },
    MaterialImported { material: Material },
    /// 非致命诊断仍然成事件（无法转换的值留在原位置）。
    ImportDiagnostic { material_id: String, diagnostics: Vec<Diagnostic> },
    SnapshotCreated { snapshot: Snapshot },
    SnapshotReleased { snapshot_id: String, seq: u64 },
    ParamsSet { params: CompactionParams },
    FlushInstalled {
        flush_id: String,
        /// 来源 WAL 记录 id。
        wal_record_ids: Vec<String>,
        /// 产出 SSTable id。
        sst_id: String,
        level: u32,
    },
    CompactionPlanned { plan: crate::core::compaction::CompactionPlan },
    CompactionDecision {
        plan_id: String,
        decision: Decision,
        reviewer: String,
        reason: String,
        /// 与既有决定冲突时记录被冲突的事件 id。
        #[serde(skip_serializing_if = "Option::is_none", default)]
        conflicts_with: Option<u64>,
    },
    CompactionInstalled {
        plan_id: String,
        installed_batch_id: String,
        output_sst_ids: Vec<String>,
        input_sst_ids: Vec<String>,
    },
    CompactionInterrupted {
        plan_id: String,
        pending_batch_id: String,
        phase: String,
    },
    RecoveryApplied {
        recovered_batch_ids: Vec<String>,
        orphaned_output_sst_ids: Vec<String>,
        reason: String,
    },
    ReadAudit {
        snapshot_seq: u64,
        key_start: Option<String>,
        key_end: Option<String>,
        /// 每个 key 的可见版本链结论。
        results: Vec<ReadResult>,
        basis: String,
    },
    BranchForked {
        from_event: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accepted,
    Rejected,
    Adjusted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResult {
    pub user_key: String,
    /// 命中的内部键身份（point 或 range）。
    pub visible_entry_id: Option<String>,
    pub visible_seq: Option<u64>,
    /// point 值 / point-tombstone / range-tombstone / 不存在。
    pub outcome: String,
    /// 负责遮蔽的 range tombstone id（若有）。
    pub covered_by_range: Option<String>,
    /// 该 key 全部版本链（按内部键顺序），均带稳定身份。
    pub version_chain: Vec<VersionLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionLink {
    pub entry_id: String,
    pub sst_id: String,
    pub seq: u64,
    pub kind: String,
    pub visible: bool,
    pub invisible_reason: Option<String>,
}
