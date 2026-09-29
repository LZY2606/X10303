//! 核心领域模型：LSM 的内部键、snapshot 可见性、范围删除覆盖。
//!
//! 所有领域量都携带单位或坐标基准：`RawValue` 保留导入时的原始表示（字符串+单位+坐标基准），
//! 只有派生层（如排序、压实）才规范化；无法转换的值留在原位置并附诊断，绝不为画图丢弃。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// 稳定身份：材料与事件在导入、导出再导入后保持不变的字符串 ID。
pub type Sid = String;

/// Sequence number（坐标基准：单调递增的逻辑序列号，单位 seq）。
pub type Seq = u64;

/// 文件大小字节数（单位 byte）。
pub type Bytes = u64;

/// 内部键排序时，同 (user_key, sequence) 多类型的类型序（v1 规则）。
///
/// 数值越大，在内部键顺序中越靠前（同一序号下优先级越高）。
/// 这是一条**可修订参数**：RocksDB 内部按类型 tag 排序，这里显式化以便审阅与反例检验。
pub const KIND_RANK_RANGE_DEL: u8 = 3;
pub const KIND_RANK_DELETE: u8 = 2;
pub const KIND_RANK_PUT: u8 = 1;

/// 参数版本号（规则版本）。
pub type RuleVersion = u32;

/// 点值类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Put,
    Delete,
}

/// 范围删除端点的包含策略（坐标基准：用户键序，end_exclusive 为 LSM 惯例）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndPolicy {
    /// [start, end)：end 不被覆盖。
    EndExclusive,
    /// [start, end]：end 也被覆盖（导入时显式声明才允许）。
    EndInclusive,
}

/// 带原始表示的领域量。
///
/// - `raw`：导入原文，逐字保留（导出再导入身份/表示不变）。
/// - `unit`：单位（byte、seq、key、bytes、entries…）。
/// - `basis`：坐标/时间基准（如 "user_key_order"、"logical_seq"、"unix_epoch_ms"）。
/// - `normalized`：派生层规范化结果；无法转换时为 `None` 并附 `diagnostic`，
///   原始值仍留在 `raw`，不会被丢弃。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawValue {
    pub raw: String,
    pub unit: String,
    pub basis: String,
    pub normalized: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl RawValue {
    pub fn new(raw: impl Into<String>, unit: impl Into<String>, basis: impl Into<String>) -> Self {
        RawValue { raw: raw.into(), unit: unit.into(), basis: basis.into(), normalized: None, diagnostic: None }
    }

    /// 解析带单位的大小（支持 512、"512B"、"4KiB"、"1MB"）。规范化为 byte。
    pub fn parse_size(raw: &str) -> Self {
        let mut v = Self::new(raw, "byte", "binary_byte_units");
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        let (num_part, mul): (&str, i64) = if let Some(p) = lower.strip_suffix("kib") {
            (p, 1024)
        } else if let Some(p) = lower.strip_suffix("mib") {
            (p, 1024 * 1024)
        } else if let Some(p) = lower.strip_suffix("kb") {
            (p, 1000)
        } else if let Some(p) = lower.strip_suffix("mb") {
            (p, 1000 * 1000)
        } else if let Some(p) = lower.strip_suffix('b') {
            (p, 1)
        } else {
            (trimmed, 1)
        };
        match num_part.trim().parse::<i64>() {
            Ok(n) if n >= 0 => v.normalized = Some(n * mul),
            _ => v.diagnostic = Some(format!("无法按 byte 解析大小: {raw:?}")),
        }
        v
    }

    pub fn size(n: u64) -> Self {
        let mut v = Self::new(n.to_string(), "byte", "binary_byte_units");
        v.normalized = Some(n as i64);
        v
    }
}

/// 一条点记录（WAL 操作 / SSTable 内 point entry）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointEntry {
    pub key: String,
    pub seq: Seq,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// 同 (key, seq, kind) 多条不同值时的稳定消歧次序（导入序），0 表示无并列。
    #[serde(default)]
    pub dup_index: u32,
}

impl PointEntry {
    pub fn put(key: &str, seq: Seq, value: &str) -> Self {
        PointEntry { key: key.to_string(), seq, kind: PointKind::Put, value: Some(value.to_string()), dup_index: 0 }
    }
    pub fn delete(key: &str, seq: Seq) -> Self {
        PointEntry { key: key.to_string(), seq, kind: PointKind::Delete, value: None, dup_index: 0 }
    }
}

/// 一条范围删除记录（tombstone 区间，坐标基准为用户键序）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeTomb {
    pub start: String,
    pub end: String,
    pub end_policy: EndPolicy,
    pub seq: Seq,
}

impl RangeTomb {
    /// 判断用户键 k 是否被该范围删除覆盖（端点语义可被审阅）。
    pub fn covers(&self, k: &str) -> bool {
        match self.end_policy {
            EndPolicy::EndExclusive => k >= self.start.as_str() && k < self.end.as_str(),
            EndPolicy::EndInclusive => k >= self.start.as_str() && k <= self.end.as_str(),
        }
    }
}

/// Snapshot（活跃读快照；读取只可见 `seq < last_seq` 的记录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: Sid,
    /// snapshot 持有的 last sequence（坐标基准：logical_seq，单位 seq）。
    pub last_seq: Seq,
    /// 是否仍活跃（未释放）。
    pub active: bool,
}

/// SSTable 摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SSTable {
    pub id: Sid,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    /// 原始大小（保留原始表示，单位 byte）。
    pub size: RawValue,
    pub points: Vec<PointEntry>,
    pub ranges: Vec<RangeTomb>,
    /// 来源：WAL+memtable flush 或压实输出（父级依赖链）。
    pub origin: FileOrigin,
}

/// 文件的来源血缘。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileOrigin {
    /// 由 WAL + memtable flush 产生。
    Flush { wal_id: Sid, flush_event: Sid },
    /// 导入时即存在的多层 SSTable 摘要（保留导入身份）。
    Imported { note: String },
    /// 某次压实运行的输出。
    Compaction { run_id: Sid, output_index: u32 },
}

impl SSTable {
    pub fn overlaps_user_range(&self, other: &SSTable) -> bool {
        !(self.largest < other.smallest || other.largest < self.smallest)
    }

    pub fn covers_key(&self, k: &str) -> bool {
        k >= self.smallest.as_str() && k <= self.largest.as_str()
    }
}

/// WAL 记录（导入材料，不可变）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalRecord {
    pub id: Sid,
    pub seq: Seq,
    pub kind: PointKind,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// WAL 文件（导入材料，不可变）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalFile {
    pub id: Sid,
    pub records: Vec<WalRecord>,
}

/// 内部键排序键：(user asc, seq desc, kind-rank desc, dup_index asc)。
///
/// 这就是“读取按内部键顺序”的那条顺序：user key 升序；同一 user key 内，
/// 新序号在前；同序号多类型按规则版本中的类型序；同类型多值按稳定 dup_index。
#[derive(Debug, Clone)]
pub struct InternalOrderKey<'a> {
    pub key: &'a str,
    pub seq: Seq,
    pub kind_rank: u8,
    pub dup_index: u32,
}

impl PartialEq for InternalOrderKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for InternalOrderKey<'_> {}
impl PartialOrd for InternalOrderKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for InternalOrderKey<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(other.key)
            .then_with(|| other.seq.cmp(&self.seq))
            .then_with(|| other.kind_rank.cmp(&self.kind_rank))
            .then_with(|| self.dup_index.cmp(&other.dup_index))
    }
}

pub fn point_kind_rank(k: PointKind) -> u8 {
    match k {
        PointKind::Put => KIND_RANK_PUT,
        PointKind::Delete => KIND_RANK_DELETE,
    }
}

/// 快照对一条记录是否可见：可见当且仅当 `record.seq < snapshot.last_seq`。
pub fn visible_to(seq: Seq, snap_last_seq: Seq) -> bool {
    seq < snap_last_seq
}

/// 返回当前所有**活跃** snapshot 中最小的 last_seq（最老读视图）；无活跃快照返回 None。
pub fn oldest_active_snapshot(snaps: &[Snapshot]) -> Option<Seq> {
    snaps.iter().filter(|s| s.active).map(|s| s.last_seq).min()
}
