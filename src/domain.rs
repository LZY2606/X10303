//! 领域模型。
//!
//! 坐标/单位基准（所有领域量都携带单位或坐标基准）：
//! - key：UTF-8 文本，按其字节序排序，键区间统一为 `[start, end)`。
//! - sequence number：uint64，单调递增，单位 "seq"。
//! - snapshot：uint64 seq，表示该快照只可见 `seq <= snapshot_seq` 的记录。
//! - level：uint，L0 允许重叠，L1+ 同层不重叠。
//! - size：字节（bytes）。
//! 原始表示（raw）原样保存在材料里，派生层才做规范化。

use serde::{Deserialize, Serialize};

pub const KEY_BASIS: &str = "utf8-lex-byte-order";
pub const SEQ_BASIS: &str = "uint64-seq";
pub const LEVEL_BASIS: &str = "uint-level;L0-overlapping;L1plus-nonoverlapping";
pub const SIZE_BASIS: &str = "bytes";

/// 内部值类型。同序号多类型时的固定次序（同 seq 时 rank 高者在内部键序中靠前）。
/// 次序是规则版本化的决定，记录在 [`Rules`] 中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VType {
    Delete,
    SingleDelete,
    Put,
    Merge,
}

impl VType {
    pub fn as_str(self) -> &'static str {
        match self {
            VType::Delete => "delete",
            VType::SingleDelete => "single_delete",
            VType::Put => "put",
            VType::Merge => "merge",
        }
    }
    /// 同 seq 内部键次序中的优先级（越大越靠前）。
    pub fn rank(self) -> u8 {
        match self {
            VType::Delete => 1,
            VType::SingleDelete => 2,
            VType::Put => 3,
            VType::Merge => 4,
        }
    }
    pub fn parse(s: &str) -> Option<VType> {
        match s.trim().to_ascii_lowercase().as_str() {
            "put" | "set" => Some(VType::Put),
            "delete" | "del" => Some(VType::Delete),
            "single_delete" | "singledelete" => Some(VType::SingleDelete),
            "merge" => Some(VType::Merge),
            _ => None,
        }
    }
    pub fn is_tombstone(self) -> bool {
        matches!(self, VType::Delete | VType::SingleDelete)
    }
}

/// 内部点记录（point value / point tombstone）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointEntry {
    pub user_key: String,
    pub seq: u64,
    pub vtype: VType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// 直接来源材料身份。
    pub origin_material: String,
    /// 该记录当前所在 SST（flush/压实安装后填充）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_sst: Option<String>,
}

/// 范围删除记录 `[start_inclusive, end_exclusive)` @ seq。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeTombstone {
    pub start_inclusive: String,
    pub end_exclusive: String,
    pub seq: u64,
    pub origin_material: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_sst: Option<String>,
}

impl RangeTombstone {
    pub fn covers(&self, k: &str) -> bool {
        self.start_inclusive.as_str() <= k && k < self.end_exclusive.as_str()
    }
    pub fn overlaps(&self, s: &str, e: &str) -> bool {
        self.start_inclusive.as_str() < e && s < self.end_exclusive.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SstOrigin {
    /// 由 memtable flush 安装（事件身份）。
    Flush(String),
    /// 由压实输出安装（派生结果身份 + 输出序号）。
    Compaction { derived: String, output_index: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SstFile {
    pub id: String,
    pub level: u32,
    /// 闭开键区间 `[min_key, max_key_exclusive)`。L1+ 同层不得重叠。
    pub min_key: String,
    pub max_key_exclusive: String,
    pub size_bytes: u64,
    pub points: Vec<PointEntry>,
    pub ranges: Vec<RangeTombstone>,
    pub origin: SstOrigin,
    /// L0 安装次序（用于多个重叠 L0 的确定性读顺序：seq 相同 tie-break）。
    pub l0_order: Option<u64>,
}

impl SstFile {
    pub fn overlaps(&self, s: &str, e: &str) -> bool {
        self.min_key.as_str() < e && s < self.max_key_exclusive.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
    pub pinned_event: String,
    pub active: bool,
}

/// 规则与参数（参数修订产生新版本；派生结果记录所用版本）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    pub version: String,
    pub label: String,
    /// 单个压实输出的字节预算；超出则必须切分（资源预算耗尽判定）。
    pub max_output_bytes: u64,
    /// 同 seq 多类型时是否允许按固定 rank 消歧；false 时遇到冲突直接语义无解。
    pub allow_same_seq_type_rank: bool,
    /// 范围墓碑仅在覆盖全部旧数据且无更旧快照时才允许下沉/丢弃。
    pub safe_drop_requires_full_coverage: bool,
    pub key_basis: String,
    pub seq_basis: String,
    pub level_basis: String,
    pub size_basis: String,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            version: "rules-v1".into(),
            label: "基线规则".into(),
            max_output_bytes: 64,
            allow_same_seq_type_rank: true,
            safe_drop_requires_full_coverage: true,
            key_basis: KEY_BASIS.into(),
            seq_basis: SEQ_BASIS.into(),
            level_basis: LEVEL_BASIS.into(),
            size_basis: SIZE_BASIS.into(),
        }
    }
}

/// 压实输入摘要（身份集合，参与压实的对象）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionInput {
    pub l0_sst_ids: Vec<String>,
    pub target_level: u32,
    /// 目标层中与输入键范围重叠、被拉入压实的 SST。
    pub target_sst_ids: Vec<String>,
}

/// 被丢弃记录及判定依据（机器可读）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedEntry {
    pub user_key: String,
    pub seq: u64,
    pub vtype: String,
    pub reason_code: String,
    pub reason: String,
    /// 使该记录可丢弃的更新记录/墓碑身份描述（origin material + sst）。
    pub shadowed_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    pub id: String,
    pub level: u32,
    pub min_key: String,
    pub max_key_exclusive: String,
    pub size_bytes: u64,
    pub point_count: usize,
    pub range_count: usize,
}

/// 一个合法的输出切分候选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitCandidate {
    pub id: String,
    /// 切分边界（end_exclusive 列表）。
    pub boundaries: Vec<String>,
    pub outputs: Vec<OutputFile>,
    pub canonical: bool,
    pub note: String,
}

/// 压实选择理由（机器可读的判定依据事件内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionReason {
    pub code: String,
    pub detail: String,
    /// 引用的证据对象身份（材料/SST/快照/规则）。
    pub evidence: Vec<String>,
}

/// 压实分析结果（纯函数产物；尚未安装）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionAnalysis {
    pub derived_id: String,
    pub rules_version: String,
    pub params_digest: String,
    pub parent_event_seq: usize,
    pub input: CompactionInput,
    /// 压实可见性下界：仍活跃的最旧快照 seq（无快照则为 0）。
    pub oldest_pinned_seq: u64,
    pub merged_min_key: String,
    pub merged_max_key_exclusive: String,
    pub surviving_points: Vec<PointEntry>,
    pub surviving_ranges: Vec<RangeTombstone>,
    pub dropped: Vec<DroppedEntry>,
    pub retained_tombstones: Vec<DroppedEntry>,
    pub candidates: Vec<SplitCandidate>,
    pub canonical_candidate_id: Option<String>,
    pub reasons: Vec<SelectionReason>,
    pub budget_bytes: u64,
    /// 保留下来的范围墓碑为何不能越过未参与压实的旧数据。
    pub range_guard: Vec<SelectionReason>,
}

/// 事件负载（档案的机器可读核心）。每个负载都含可复算的摘要与引用身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "etype", rename_all = "snake_case")]
pub enum EventPayload {
    /// 一次导入批次（WAL/flush/SST 摘要/快照 pin/历史压实摘要）。
    ImportBatch {
        batch_id: String,
        rules_version: String,
        material_ids: Vec<String>,
        material_kinds: Vec<String>,
        key_basis: String,
        seq_basis: String,
        size_basis: String,
    },
    /// memtable flush：WAL 记录 -> 新 L0 SST。
    Flush {
        sst_id: String,
        level: u32,
        source_material_ids: Vec<String>,
        point_seqs: Vec<u64>,
        range_seqs: Vec<u64>,
        min_key: String,
        max_key_exclusive: String,
        size_bytes: u64,
        l0_order: u64,
    },
    SnapshotPinned { snapshot_id: String, seq: u64 },
    SnapshotReleased { snapshot_id: String },
    /// 压实分析完成但尚未安装（interrupted 也作为持久事件留下）。
    CompactionPrepared {
        derived_id: String,
        analysis: CompactionAnalysis,
        interrupted: bool,
    },
    /// 安装（提交）压实输出：消费 inputs，产出 outputs。幂等键 derived_id。
    CompactionInstalled {
        derived_id: String,
        chosen_candidate_id: String,
        consumed_sst_ids: Vec<String>,
        output_sst_ids: Vec<String>,
    },
    /// 重复安装尝试被拒绝（幂等证据，不是错误）。
    CompactionInstallDuplicate { derived_id: String, existing_event_id: String },
    RulesRevised {
        from_version: String,
        rules: Rules,
        change_summary: String,
    },
    DecisionAccepted {
        derived_id: String,
        candidate_id: String,
        note: String,
    },
    DecisionRejected {
        derived_id: String,
        candidate_id: String,
        note: String,
    },
    BranchForked { name: String, parent_branch: String, at_seq: usize },
}

impl EventPayload {
    pub fn etype_str(&self) -> &'static str {
        match self {
            EventPayload::ImportBatch { .. } => "import_batch",
            EventPayload::Flush { .. } => "flush",
            EventPayload::SnapshotPinned { .. } => "snapshot_pinned",
            EventPayload::SnapshotReleased { .. } => "snapshot_released",
            EventPayload::CompactionPrepared { .. } => "compaction_prepared",
            EventPayload::CompactionInstalled { .. } => "compaction_installed",
            EventPayload::CompactionInstallDuplicate { .. } => "compaction_install_duplicate",
            EventPayload::RulesRevised { .. } => "rules_revised",
            EventPayload::DecisionAccepted { .. } => "decision_accepted",
            EventPayload::DecisionRejected { .. } => "decision_rejected",
            EventPayload::BranchForked { .. } => "branch_forked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub branch: String,
    pub seq: usize,
    pub at_event_parent: String,
    pub payload: EventPayload,
}
