//! 领域内核：层合迹（LSM tombstone / 范围删除 / 压实选择）的全部业务概念。
//!
//! 分层原则：
//! - [`Material`] 是不可变材料（WAL、memtable flush、多层 SSTable 摘要、snapshot、
//!   原始量值表示），导入后永不被覆盖。
//! - [`Decision`]/[`Event`] 是可修改的决定层：参数修订、人工接受/否定、压实安装，
//!   全部以追加事件记录，旧结论可按事件回溯恢复。
//! - 所有领域量都携带单位或坐标基准（[`Measure`]）；导入保留 raw，派生层才规范化，
//!   无法转换的值留在原位置并附 [`Diagnostic`]。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// 逻辑序列号（sequence number），单调递增；坐标基准：本地序号空间 `local-seq-space`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Seq(pub u64);

/// 压实/读取判定的机器可读结果类别。资源预算、格式损坏、语义无解是三种不同结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// 计算成功完成（输出可能被切分为多个 SSTable）。
    Ok,
    /// 资源预算（压实 IO 字节预算）耗尽，给出已消耗预算与断点位置，不产生已安装输出。
    BudgetExhausted {
        budget_bytes: u64,
        consumed_bytes: u64,
        last_position: String,
    },
    /// 语义无解：存在多个合法候选但无法唯一决定（候选全部保留）。
    NoSolution { reason: String, candidates: Vec<String> },
    /// 格式损坏：给出字节/字段位置；存储层保证不会落库为成功状态。
    Corrupt { location: String, detail: String },
}

/// 带位置的诊断；输入不完整或不可转换时返回它，绝不用空值/默认值伪装成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub location: String,
    pub message: String,
    /// trace | warning | error：error 必须阻断对应写入，warning 保留原值并标记。
    pub severity: String,
    pub raw_value: Option<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, location: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic { code: code.into(), location: location.into(), message: message.into(), severity: "error".into(), raw_value: None }
    }
    pub fn warn(code: impl Into<String>, location: impl Into<String>, message: impl Into<String>, raw: Option<String>) -> Self {
        Diagnostic { code: code.into(), location: location.into(), message: message.into(), severity: "warning".into(), raw_value: raw }
    }
}

/// 领域量：保留原始表示 + 单位/坐标基准，规范化值是派生层产物。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measure {
    /// 导入时的原始表示，逐字保留（例如 "12", "12KB", "big"）。
    pub raw: String,
    /// 单位：bytes / microseconds / sequence-number / count ...
    pub unit: String,
    /// 坐标或时间基准：utf8:byte-lex / wall-unix-epoch-us / local-seq-space ...
    pub basis: String,
    /// 派生规范化值；无法转换时为 None，raw 仍保留并附诊断。
    #[serde(default)]
    pub normalized: Option<i64>,
}

impl Measure {
    pub fn bytes(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let normalized = parse_bytes(&raw);
        Measure { raw, unit: "bytes".into(), basis: "iec-binary-prefix".into(), normalized }
    }
    pub fn seq(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let normalized = raw.trim().parse::<i64>().ok();
        Measure { raw, unit: "sequence-number".into(), basis: "local-seq-space".into(), normalized }
    }
    pub fn micros(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let normalized = raw.trim().parse::<i64>().ok();
        Measure { raw, unit: "microseconds".into(), basis: "wall-unix-epoch-us".into(), normalized }
    }
    pub fn count(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let normalized = raw.trim().parse::<i64>().ok();
        Measure { raw, unit: "count".into(), basis: "decimal".into(), normalized }
    }
}

/// 解析 "12" / "12KB" / "4 MiB" 等字节表示；无法转换返回 None（保留 raw + 诊断）。
pub fn parse_bytes(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let (num, mul) = if let Some(n) = s.strip_suffix("KiB") {
        (n.trim(), 1024i64)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n.trim(), 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n.trim(), 1000)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n.trim(), 1_000_000)
    } else if let Some(n) = s.strip_suffix('B') {
        (n.trim(), 1)
    } else {
        (s, 1)
    };
    num.parse::<i64>().ok().map(|n| n.saturating_mul(mul))
}

/// 用户键：原始表示 + 坐标基准；排序键是派生值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomKey {
    /// 原始键表示，逐字保留。
    pub raw: String,
    /// 坐标基准，决定如何规范化排序，例如 utf8:byte-lex / decimal:numeric。
    pub coordinate: String,
}

impl DomKey {
    pub fn utf8(raw: impl Into<String>) -> Self {
        DomKey { raw: raw.into(), coordinate: "utf8:byte-lex".into() }
    }
    /// 派生排序字节；坐标基准无法识别时返回 None（留在原位置 + 诊断，不参与画图）。
    pub fn order_bytes(&self) -> Option<Vec<u8>> {
        match self.coordinate.as_str() {
            "utf8:byte-lex" => Some(self.raw.as_bytes().to_vec()),
            "decimal:numeric" => self.raw.trim().parse::<i64>().ok().map(|n| n.to_be_bytes().to_vec()),
            _ => None,
        }
    }
    /// 按坐标基准比较；不可比较时返回 None（由调用方记诊断，不静默丢弃）。
    pub fn compare(&self, other: &DomKey) -> Option<Ordering> {
        let (a, b) = (self.order_bytes()?, other.order_bytes()?);
        Some(a.cmp(&b))
    }
}

/// 记录类型：点值或点删除（point tombstone）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Put,
    Delete,
}

/// 来源坐标：任意一条记录都能反查到不可变材料。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginRef {
    /// wal / sst / compaction-output
    pub source_kind: String,
    /// 来源材料稳定身份（wal_* / sst_* / out_*）。
    pub source_id: String,
    /// 材料内位置（例如 WAL 偏移、点序号），用于带位置诊断。
    pub position: usize,
}

/// 一条点记录（内部键 = user key + sequence + type）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointRecord {
    pub seq: Seq,
    pub kind: EntryKind,
    /// point value 原始表示；Delete 时为 None（删除记录单独成义，不用空串伪装）。
    pub value: Option<String>,
    pub origin: OriginRef,
}

/// 范围删除端点（半开区间 [start, end)，端点坐标随键一起保留）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeEndpoint {
    pub key: DomKey,
    pub inclusive: bool,
}

/// 范围删除记录（range tombstone）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeTombstone {
    pub id: String,
    pub seq: Seq,
    pub start: RangeEndpoint,
    pub end: RangeEndpoint,
    pub origin: OriginRef,
}

impl RangeTombstone {
    /// 键 k 是否落在删除范围内（端点包含性显式判定，供“范围删除端点”情形取证）。
    pub fn covers(&self, k: &DomKey) -> Option<bool> {
        let after_start = match k.compare(&self.start.key) {
            Some(Ordering::Greater) => true,
            Some(Ordering::Equal) => self.start.inclusive,
            Some(Ordering::Less) => false,
            None => return None,
        };
        let before_end = match k.compare(&self.end.key) {
            Some(Ordering::Less) => true,
            Some(Ordering::Equal) => self.end.inclusive,
            Some(Ordering::Greater) => false,
            None => return None,
        };
        Some(after_start && before_end)
    }
}

/// WAL 条目：导入 WAL 阶段的不可变材料。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry {
    pub id: String,
    pub seq: Seq,
    pub key: DomKey,
    pub kind: EntryKind,
    pub value: Option<String>,
    /// 写入时间戳（携带时间基准 wall-unix-epoch-us）。
    pub written_at: Measure,
    /// WAL 字节偏移（材料内坐标）。
    pub wal_offset: usize,
}

/// memtable flush 摘要：哪些 WAL 条目经 flush 产出哪个 SSTable。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushSummary {
    pub id: String,
    pub wal_entry_ids: Vec<String>,
    pub produced_sst_id: String,
    pub at: Measure,
}

/// 多层 SSTable 摘要（含点记录、范围删除、key range、层级、字节量纲）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SstSummary {
    pub id: String,
    pub file_no: u64,
    pub level: u32,
    pub smallest: DomKey,
    pub largest: DomKey,
    pub size: Measure,
    pub points: Vec<PointRecord>,
    pub ranges: Vec<RangeTombstone>,
    /// import / flush:<flush_id> / compaction:<plan_id>
    pub produced_by: String,
}

impl SstSummary {
    pub fn overlaps_range(&self, lo: &DomKey, hi: &DomKey) -> Option<bool> {
        let left = self.largest.compare(lo)?;
        let right = self.smallest.compare(hi)?;
        Some(left != Ordering::Less && right != Ordering::Greater)
    }
    pub fn overlaps(&self, other: &SstSummary) -> Option<bool> {
        self.overlaps_range(&other.smallest, &other.largest)
    }
}

/// snapshot（读取可见性基准）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: Seq,
    pub active: bool,
    pub label: String,
}

/// 压实参数（可被修订；每个版本不可变，结果记录所用版本）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionParams {
    pub version: String,
    pub output_max_bytes: u64,
    pub compaction_io_budget_bytes: u64,
    /// 是否允许在输出切分时把 range tombstone 碎片复制到多个输出。
    pub allow_range_fragmentation: bool,
    pub note: String,
}

/// 不可变材料包：初始导入后永不修改；源数据改动以“增补”形式追加。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Materials {
    pub case_id: String,
    pub title: String,
    pub narrative: Vec<String>,
    pub wal: Vec<WalEntry>,
    pub flushes: Vec<FlushSummary>,
    pub ssts: Vec<SstSummary>,
    pub snapshots: Vec<Snapshot>,
    /// 案例对各边界情形的材料说明（领域材料，非通用表格）。
    pub scenario_notes: Vec<ScenarioNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioNote {
    pub code: String,
    pub title: String,
    pub detail: String,
}

/// 内部键排序：同 user key 按 seq 降序；同 seq 时 Delete 在 Put 之前
/// （沿用 RocksDB value-type tie-break：较小的 type 排序在前），同型同序按 value 字节序，
/// 并由调用方把“同序号多类型/多值”标记为歧义候选。
pub fn internal_cmp(a: &PointRecord, ak: &DomKey, b: &PointRecord, bk: &DomKey) -> Option<Ordering> {
    match ak.compare(bk)? {
        Ordering::Equal => match b.seq.cmp(&a.seq) {
            Ordering::Equal => {
                let ta = type_rank(a.kind);
                let tb = type_rank(b.kind);
                Some(match ta.cmp(&tb) {
                    Ordering::Equal => a.value.cmp(&b.value),
                    o => o,
                })
            }
            o => Some(o),
        },
        o => Some(o),
    }
}

fn type_rank(k: EntryKind) -> u8 {
    match k {
        EntryKind::Delete => 0,
        EntryKind::Put => 1,
    }
}
