//! 不可变材料与领域值对象。
//!
//! 所有领域量都显式携带单位或坐标基准；无法规范化的值保留原始表示并附诊断,
//! 派生层绝不为了画图而丢掉原值。

use serde::{Deserialize, Serialize};

pub type Json = serde_json::Value;

/// 带位置的诊断 (JSON Pointer 指向来源片段)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub code: String,
    pub severity: DiagSeverity,
    /// 来源包内位置, 例如 `/sstables/3/entries/1/seq`。
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DiagSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PointKind {
    Put,
    Delete,
}

impl PointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PointKind::Put => "put",
            PointKind::Delete => "delete",
        }
    }
    /// 内部键同序号并列时的类型序 (RocksDB 风格: delete=1 在 put=0 之前)。
    pub fn type_order(self) -> u8 {
        match self {
            PointKind::Delete => 1,
            PointKind::Put => 0,
        }
    }
}

/// 带原始表示的标量值。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawScalar {
    /// 来源中的原始 JSON 表示, 原样保存。
    pub raw: Json,
    pub basis: String,
    pub normalized: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub diagnostic: Option<Diagnostic>,
}

/// WAL 中的一条记录 (导入后即视为 memtable 可见内容)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalRecord {
    pub id: String,
    pub seq: u64,
    pub key: String,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalFile {
    pub id: String,
    pub name: String,
    pub key_encoding: String,
    pub records: Vec<WalRecord>,
}

/// memtable flush 摘要: 连接 WAL 与产出的 L0 SSTable。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlushSummary {
    pub id: String,
    pub wal_id: String,
    pub produced_sst: String,
    pub seq_lo: u64,
    pub seq_hi: u64,
    /// 原始时间表示, 原样保留。
    pub at_raw: Json,
    pub time_basis: String,
}

/// 点数据条目 (Put 值或 point tombstone)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PointEntry {
    pub id: String,
    pub seq: u64,
    pub key: String,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
}

/// 范围删除 (端点含/斥标志属于坐标基准的一部分, 原始表示长期保留)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RangeTombstone {
    pub id: String,
    pub seq: u64,
    pub start_key: String,
    pub start_inclusive: bool,
    pub end_key: String,
    pub end_inclusive: bool,
    pub bound_basis: String,
}

/// 文件大小: 原始表示 + 单位 + 规范化字节数 (无法转换则留原位置并附诊断)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileSize {
    pub raw: Json,
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub normalized_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub diagnostic: Option<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sstable {
    pub id: String,
    pub file: String,
    pub level: u8,
    pub key_min: String,
    pub key_max: String,
    pub size: FileSize,
    pub born_seq: u64,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin_ref: Option<String>,
    #[serde(default)]
    pub entries: Vec<PointEntry>,
    #[serde(default)]
    pub range_dels: Vec<RangeTombstone>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub label: String,
    /// 快照可见上界: 仅 seq <= snapshot_seq 的内部键可见。
    pub seq: u64,
}

/// 参数 (规则) 集。不可变值对象, 修订即产生新版本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleSet {
    pub version: String,
    /// 同 (user key, seq) 多类型冲突时的裁决策略。
    pub same_seq_tiebreak: SameSeqTiebreak,
    /// point tombstone 下沉丢弃条件。
    pub point_tombstone_drop: String,
    /// range tombstone 下沉丢弃条件。
    pub range_tombstone_drop: String,
    /// 输出层必须保持非重叠。
    pub output_level_nonoverlap: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SameSeqTiebreak {
    /// 不强行裁决, 保留全部合法候选 (语义无解时显式上报)。
    AllCandidates,
    DeleteWins,
    PutWins,
}

/// 不可变材料包 (WAL / flush 摘要 / 多层 SSTable / snapshot)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Materials {
    pub pack_id: String,
    pub pack_version: String,
    pub key_encoding: String,
    pub seq_basis: String,
    pub time_basis: String,
    #[serde(default)]
    pub wals: Vec<WalFile>,
    #[serde(default)]
    pub flushes: Vec<FlushSummary>,
    #[serde(default)]
    pub sstables: Vec<Sstable>,
    #[serde(default)]
    pub snapshots: Vec<Snapshot>,
}
