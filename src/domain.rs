//! 核心领域类型：内部键、point/range tombstone、snapshot、文件摘要。
//!
//! 所有领域量在原始材料中保留原始表示（字符串 + 单位/基准），
//! 规范化只发生在派生层（[crate::lsm]）。

use serde::{Deserialize, Serialize};

/// 序列号（LSM internal sequence number）。
pub type Seq = i64;

/// 内部键排序中，同序号不同类型的稳定 tie-break。
///
/// 数值越大在内部键顺序中越靠前：RocksDB 风格 put(0)/delete(1)/single-delete(3)
/// 之外，本审阅工具还需要 range 标记；同序号多类型语义无解时使用
/// [`TieBreak::Conflict`] 并保留全部合法候选。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TieBreak {
    Put = 0,
    Merge = 2,
    Delete = 1,
    SingleDelete = 3,
    RangeStart = 5,
    RangeEnd = 4,
    /// 同序号同 key 出现互斥多类型，且没有规则能唯一裁决。
    Conflict = 9,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Put,
    Merge,
    Delete,
    SingleDelete,
}

impl PointKind {
    pub fn tie(self) -> TieBreak {
        match self {
            PointKind::Put => TieBreak::Put,
            PointKind::Merge => TieBreak::Merge,
            PointKind::Delete => TieBreak::Delete,
            PointKind::SingleDelete => TieBreak::SingleDelete,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            PointKind::Put => "put",
            PointKind::Merge => "merge",
            PointKind::Delete => "delete",
            PointKind::SingleDelete => "single_delete",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "put" => PointKind::Put,
            "merge" => PointKind::Merge,
            "delete" => PointKind::Delete,
            "single_delete" | "sdelete" => PointKind::SingleDelete,
            _ => return None,
        })
    }
}

/// 一条 point 记录。`value` 为 None 表示墓碑（delete / single_delete）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct PointEntry {
    pub user_key: String,
    pub seq: Seq,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
    /// 原始表示（导入时保留，例如 "0x07" 或 "42#byte"）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw_value: Option<String>,
    /// 值的单位/坐标基准，未声明为 None（不是空串默认值）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unit: Option<String>,
}

impl PointEntry {
    pub fn internal_key(&self) -> InternalKey<'_> {
        InternalKey {
            user_key: &self.user_key,
            seq: self.seq,
            tb: self.kind.tie(),
        }
    }
    pub fn is_tombstone(&self) -> bool {
        matches!(self.kind, PointKind::Delete | PointKind::SingleDelete)
    }
}

/// 内部键比较器：(user_key 升序, seq 降序, tie-break 降序)。
#[derive(Debug, Clone, Copy)]
pub struct InternalKey<'a> {
    pub user_key: &'a str,
    pub seq: Seq,
    pub tb: TieBreak,
}

impl<'a> InternalKey<'a> {
    pub fn cmp_ik(a: InternalKey<'_>, b: InternalKey<'_>) -> std::cmp::Ordering {
        a.user_key
            .cmp(b.user_key)
            .then(b.seq.cmp(&a.seq))
            .then((b.tb as u8).cmp(&(a.tb as u8)))
    }
}

pub fn sort_internal(entries: &mut [PointEntry]) {
    entries.sort_by(|a, b| InternalKey::cmp_ik(a.internal_key(), b.internal_key()));
}

/// 范围删除的端点包含性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Inclusive,
    Exclusive,
}

/// 范围删除记录（RocksDB range tombstone: [start, end) 风格，端点可声明）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct RangeTombstone {
    pub id: String,
    pub start_key: String,
    pub start_kind: EndpointKind,
    pub end_key: String,
    pub end_kind: EndpointKind,
    pub seq: Seq,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw: Option<String>,
}

impl RangeTombstone {
    pub fn covers(&self, key: &str) -> bool {
        let left = match self.start_kind {
            EndpointKind::Inclusive => key >= self.start_key.as_str(),
            EndpointKind::Exclusive => key > self.start_key.as_str(),
        };
        let right = match self.end_kind {
            EndpointKind::Inclusive => key <= self.end_key.as_str(),
            EndpointKind::Exclusive => key < self.end_key.as_str(),
        };
        left && right
    }
    pub fn overlaps_interval(&self, lo: &str, hi: &str) -> bool {
        // 空区间不重叠
        if !(lo <= hi) {
            return false;
        }
        let tomb_lo = &self.start_key;
        let tomb_hi = &self.end_key;
        // 简化为闭区间比较（包含/排除端点的细微差别在端点相等场景由 covers 判定）
        !(tomb_hi < lo || tomb_lo > hi)
    }
}

/// 读快照。`last_seq` = 该 snapshot 可见的最大序列号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct Snapshot {
    pub id: String,
    pub last_seq: Seq,
    /// 时间基准：原始表示（如 "wall:2026-09-25T07:00:00Z#utc"）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub taken_at_raw: Option<String>,
    pub active: bool,
}

/// SSTable 文件摘要。entries 是该文件携带的 point 记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct SstFile {
    pub file_id: String,
    pub level: u32,
    /// 声明的 key range（原始字符串坐标）。smallest/largest 均含端点。
    pub smallest: String,
    pub largest: String,
    pub entries: Vec<PointEntry>,
    pub range_tombstones: Vec<RangeTombstone>,
    /// 文件大小（派生切分使用），带单位原始表示。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size_raw: Option<String>,
    /// 规范化字节数；无法转换时为 None 并保留 size_raw + 诊断。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size_bytes: Option<u64>,
    pub installed_at_seq: Seq,
}

impl SstFile {
    pub fn key_range_covers(&self, key: &str) -> bool {
        key >= self.smallest.as_str() && key <= self.largest.as_str()
    }
}

/// memtable（flush 的输入/产物快照）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct Memtable {
    pub id: String,
    pub entries: Vec<PointEntry>,
    pub range_tombstones: Vec<RangeTombstone>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub flushed_to: Option<String>,
}

/// 一条 WAL 记录（不可变材料）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct WalRecord {
    pub offset: u64,
    pub entry: PointEntry,
}

/// 压实参数（规则版本）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct CompactionParams {
    pub version: String,
    /// 输出文件目标最大字节；超过则切分。
    pub max_output_bytes: u64,
    /// 是否允许在非最底层丢弃被旧 snapshot 保护的墓碑（永远应为 false；
    /// 作为可审计参数保留，冲突决定场景会提议 true）。
    pub drop_under_snapshot: bool,
    /// L0 触发压实的重叠文件数（仅影响选择理由展示）。
    pub l0_compaction_trigger: usize,
    /// 切分 tie-break 策略：`stable`（按内部键确定性切）或
    /// `first_feasible`（从左扫描的首个可行点，等价输入仍稳定）。
    pub split_policy: String,
}

impl Default for CompactionParams {
    fn default() -> Self {
        CompactionParams {
            version: "rules/v1".to_string(),
            max_output_bytes: 64,
            drop_under_snapshot: false,
            l0_compaction_trigger: 2,
            split_policy: "stable".to_string(),
        }
    }
}
