//! 领域模型：内部键、point/range 记录、SSTable 摘要、快照、参数、材料与事件。
//!
//! 所有领域量都携带单位或坐标基准（见各字段名 / `raw` 表示）。
//! 导入时保留原始表示（`*_raw`），规范化只发生在派生层；无法转换的值留在原位置并附诊断。

use serde::{Deserialize, Serialize};

/// 坐标基准 / 单位声明。序列化为字符串，例如 "byte"、"key-order:unsigned-lexicographic"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordBasis {
    pub dimension: String,
    pub unit: String,
    /// 编码时使用的基准，例如 lexicographic(utf8) / epoch-ms。
    pub encoding: String,
}

/// 一个带原始表示的领域量。`parsed` 仅在派生时使用；无法转换时为 None 并保留诊断。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawValue {
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parsed: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

impl RawValue {
    pub fn raw_only(raw: impl Into<String>) -> Self {
        RawValue { raw: raw.into(), parsed: None, diagnostics: Vec::new() }
    }
    pub fn with_parse(raw: impl Into<String>, parsed: f64) -> Self {
        RawValue { raw: raw.into(), parsed: Some(parsed), diagnostics: Vec::new() }
    }
}

/// 带位置的诊断。position 指向导入载荷中的 JSON 指针样式位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub position: String,
    pub message: String,
    /// fatal=true 时整个导入事务回滚；false 时值保留在原位置。
    pub fatal: bool,
}

/// point 操作类型。同 user key + 同 sequence 时按内部键排序定义取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PointKind {
    Put,
    Delete,
    SingleDelete,
}

impl PointKind {
    /// 同一 (user key, sequence) 上多类型共存时的内部键 tie-break：
    /// 数值越大在内部键顺序中越靠前；读取时取最靠前且可见的一条。
    pub fn internal_rank(self) -> u8 {
        match self {
            // RocksDB 惯例：相同序列号下 DELETE 排在 PUT 之前。
            PointKind::SingleDelete => 3,
            PointKind::Delete => 2,
            PointKind::Put => 1,
        }
    }
    pub fn is_tombstone(self) -> bool {
        matches!(self, PointKind::Delete | PointKind::SingleDelete)
    }
}

/// point 记录。值以原始字节（UTF-8 文本承载）保留，大小带单位。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointEntry {
    /// 稳定身份：材料内唯一 id，导入时保留。
    pub id: String,
    pub user_key: String,
    pub seq: u64,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
    /// 值大小，带 byte 单位与原始表示。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value_size: Option<RawValue>,
    /// 有效区间坐标基准（key-order），由派生层填充，供 UI 展示。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

/// 范围删除（range tombstone）。半开区间 [start, end)，坐标基准为 key 顺序。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeTombstone {
    pub id: String,
    pub start: String,
    pub end: String,
    pub seq: u64,
    /// 端点约定，固定为 half-open [start,end)，明确写出坐标基准。
    pub endpoint_basis: String,
    /// 原始端点表示（导入保留）。
    pub start_raw: String,
    pub end_raw: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

impl RangeTombstone {
    pub fn contains(&self, key: &str) -> bool {
        // 半开：start <= key < end
        key >= self.start.as_str() && key < self.end.as_str()
    }
}

/// SSTable 摘要：身份、层级、key range、point 版本与范围删除全部保留。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SstSummary {
    pub id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    /// key 范围坐标基准。
    pub key_basis: String,
    pub points: Vec<PointEntry>,
    pub range_deletes: Vec<RangeTombstone>,
    /// 文件大小（byte，带原始表示）。
    pub file_size: RawValue,
    pub origin: SstOrigin,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SstOrigin {
    /// 外部材料直接导入。
    Imported,
    /// memtable flush 产物。
    Flush,
    /// compaction 输出。
    Compaction,
}

impl SstSummary {
    pub fn overlaps_range(&self, start: &str, end_exclusive: &str) -> bool {
        // 文件 [smallest, largest] 与 [start,end) 相交（largest 按包含处理，end 按开区间）。
        !(self.largest.as_str() < start || self.smallest.as_str() >= end_exclusive)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        key >= self.smallest.as_str() && key <= self.largest.as_str()
    }
}

/// WAL 中的一条变更帧（结构化 JSON 导入路径使用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRecord {
    pub id: String,
    pub seq: u64,
    pub kind: PointKind,
    pub user_key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value_size: Option<RawValue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

/// 快照。active=true 时作为读取屏障与墓碑保留屏障。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
    pub created_event: Option<u64>,
    pub released_event: Option<u64>,
    pub time_basis: String,
    /// 创建时间原始表示。
    pub created_raw: String,
}

/// compaction 参数（可修订；带版本）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionParams {
    pub version: u32,
    /// 输出切分预算（byte）。
    pub max_output_bytes: u64,
    /// L0 触发 compaction 的文件数阈值。
    pub l0_trigger: u32,
    /// L0 同分时策略：first / reject_ambiguous。
    pub tie_policy: String,
    pub size_unit: String,
}

impl Default for CompactionParams {
    fn default() -> Self {
        CompactionParams {
            version: 1,
            max_output_bytes: 200,
            l0_trigger: 2,
            tie_policy: "first".to_string(),
            size_unit: "byte".to_string(),
        }
    }
}

/// 不可变导入材料。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Material {
    pub id: String,
    pub kind: MaterialKind,
    /// 原始载荷的稳定摘要（内容寻址）。
    pub content_hash: String,
    pub imported_event: u64,
    pub wal: Vec<WalRecord>,
    pub ssts: Vec<SstSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialKind {
    /// 存储团队导入包：WAL + 已存在的多层 SSTable 摘要。
    StorageIngest,
    /// 半写 WAL 帧（二进制/文本帧路径）。
    WalFrames,
    /// 案例包本地源数据改动。
    CasePatch,
}
