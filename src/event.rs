//! 只追加事件日志：命令、决定、派生产物与证据都以事件形式持久化。
//! 事件通过 `prev_hash -> hash` 形成哈希链，导出再导入后校验完整性。

use crate::types::ComputeOutcome;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 机器可读判定依据：谓词 + 命名参数，供界面逐条渲染/复核。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub predicate: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// 受影响对象引用（每个显示值都能反查稳定身份）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Affected {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 结构化证据：判定 + 依据谓词 + 受影响对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub evidence_id: String,
    pub title: String,
    /// 判定类别，如 selection / visibility / tombstone_retain / split / nonoverlap /
    /// budget / semantic_conflict / install / interrupt / amendment。
    pub class: String,
    pub basis: Vec<Fact>,
    pub affected: Vec<Affected>,
}

/// 一次命令的执行结果：输入摘要、参数版本、父级依赖、产物、证据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResultRecord {
    #[serde(default)]
    pub input_summary: Vec<String>,
    #[serde(default)]
    pub rules_version: Option<u32>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub produced: Vec<String>,
    #[serde(default)]
    pub superseded: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 所有支持的命令（即材料与决定分层中的“可修改决定”）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    OpenCase {
        case_id: String,
        title: String,
        basis: crate::types::Basis,
    },
    ImportWal {
        material_id: String,
        source_label: String,
        lines: Vec<String>,
    },
    FlushWal {
        material_id: String,
        sst_id: String,
        /// 参与 flush 的原始行偏移区间（含），跳过损坏/不完整窗口由结果记录。
        windows: Vec<(usize, usize)>,
    },
    ImportSst {
        material_id: String,
        sst_id: String,
        level: u32,
        source_label: String,
        entries: Vec<crate::types::Entry>,
        declared_size: Option<String>,
    },
    OpenSnapshot {
        snap_id: String,
        seq: crate::types::Seq,
    },
    CloseSnapshot {
        snap_id: String,
    },
    ReviseRules {
        new_version: u32,
        budget: String,
        hard_budget: bool,
        policy: String,
        note: String,
    },
    RequestCompaction {
        job_id: String,
        seed_sst: String,
        target_level: Option<u32>,
    },
    /// 安装已暂存的压实；resume_mode 用于中断/恢复与重复安装检验。
    InstallCompaction {
        job_id: String,
        /// first_time | rollforward | rollback | duplicate
        resume_mode: String,
    },
    /// 人工在语义冲突的合法候选中做出决定（记录在所在分支）。
    ResolveConflict {
        job_id: String,
        candidate: String,
    },
    /// 只改动一个直接来源（材料修订），随后按依赖链重算派生层。
    AmendMaterial {
        old_material_id: String,
        new_material_id: String,
        lines: Option<Vec<String>>,
        entries: Option<Vec<crate::types::Entry>>,
        declared_size: Option<String>,
        change_note: String,
    },
    CreateBranch {
        branch: String,
        from_event_id: String,
        reason: String,
    },
    Checkpoint {
        label: String,
    },
}

impl Command {
    pub fn kind(&self) -> &'static str {
        match self {
            Command::OpenCase { .. } => "open_case",
            Command::ImportWal { .. } => "import_wal",
            Command::FlushWal { .. } => "flush_wal",
            Command::ImportSst { .. } => "import_sst",
            Command::OpenSnapshot { .. } => "open_snapshot",
            Command::CloseSnapshot { .. } => "close_snapshot",
            Command::ReviseRules { .. } => "revise_rules",
            Command::RequestCompaction { .. } => "request_compaction",
            Command::InstallCompaction { .. } => "install_compaction",
            Command::ResolveConflict { .. } => "resolve_conflict",
            Command::AmendMaterial { .. } => "amend_material",
            Command::CreateBranch { .. } => "create_branch",
            Command::Checkpoint { .. } => "checkpoint",
        }
    }
}

/// 持久化事件信封。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub clock: u64,
    pub case_id: String,
    pub branch: String,
    pub kind: String,
    pub command: Command,
    /// 执行结果（成功/预算/语义冲突的三分支序列化在 outcome 内）。
    pub outcome: ComputeOutcome<ResultRecord>,
    /// 本分支前一事件 id。
    pub parent_event_id: Option<String>,
    /// 分叉点事件 id（若为分支首事件）。
    pub fork_from_event_id: Option<String>,
    pub prev_hash: String,
    pub hash: String,
}

/// FNV-1a 64：确定性、无外部依赖；仅用于完整性链，不作为安全哈希。
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 计算事件内容哈希（不含 hash 字段本身）。
pub fn content_hash(e: &Event) -> String {
    let mut v = BTreeMap::new();
    v.insert("event_id", serde_json::to_value(&e.event_id).unwrap());
    v.insert("clock", serde_json::to_value(e.clock).unwrap());
    v.insert("case_id", serde_json::to_value(&e.case_id).unwrap());
    v.insert("branch", serde_json::to_value(&e.branch).unwrap());
    v.insert("kind", serde_json::to_value(&e.kind).unwrap());
    v.insert("command", serde_json::to_value(&e.command).unwrap());
    v.insert("outcome", serde_json::to_value(&e.outcome).unwrap());
    v.insert("parent_event_id", serde_json::to_value(&e.parent_event_id).unwrap());
    v.insert("fork_from_event_id", serde_json::to_value(&e.fork_from_event_id).unwrap());
    v.insert("prev_hash", serde_json::to_value(&e.prev_hash).unwrap());
    let bytes = serde_json::to_vec(&v).expect("canonical json");
    format!("{:016x}", fnv1a(&bytes))
}
