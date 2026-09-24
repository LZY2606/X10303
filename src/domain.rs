//! LSM 领域模型：内部键、记录、SSTable、层级、快照、压实参数。
//! 所有领域量携带单位或坐标基准；导入保留原始表示，派生层才做规范化。

use serde::{Deserialize, Serialize};
use std::fmt;

// 内部键序按 kind 降序：PUT 必须先于同序号的 DEL（与 RocksDB 的 kTypeValue > kTypeDeletion 一致）
pub const KIND_DELETE: u8 = 0;
pub const KIND_POINT: u8 = 1;
pub const KIND_RANGE_DELETE: u8 = 2;

/// 内部键：用户键 + 序号（降序）+ 类型（降序）。
/// 排序规则与 RocksDB 一致：user_key 升序，seq 降序，kind 降序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalKey {
    pub user_key: String,
    pub seq: u64,
    pub kind: u8,
}

impl InternalKey {
    pub fn new(user_key: &str, seq: u64, kind: u8) -> Self {
        InternalKey { user_key: user_key.to_string(), seq, kind }
    }
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.user_key
            .cmp(&other.user_key)
            .then(other.seq.cmp(&self.seq))
            .then(other.kind.cmp(&self.kind))
    }
}
impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for InternalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            KIND_POINT => "PUT",
            KIND_DELETE => "DEL",
            KIND_RANGE_DELETE => "RDEL",
            _ => "?",
        };
        write!(f, "{}#{}#{}", self.user_key, self.seq, kind)
    }
}

/// 带单位/坐标基准的领域量。导入时保留原始表示；无法转换时留在原位置并附诊断。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quantity {
    /// 原始文本表示（如 "4096B"、"1ms"），导入时原样保留
    pub raw: String,
    /// 规范化后的数值（派生层产物）
    pub value: Option<f64>,
    /// 单位或坐标基准（如 "bytes"、"seq"、"ms"、"user-key-space"）
    pub unit: String,
    /// 无法转换时的诊断信息；存在时 value 为 None，不得用默认值伪装成功
    pub diagnostic: Option<String>,
}

impl Quantity {
    pub fn bytes(v: u64) -> Self {
        Quantity { raw: format!("{}B", v), value: Some(v as f64), unit: "bytes".into(), diagnostic: None }
    }
    pub fn seq(v: u64) -> Self {
        Quantity { raw: format!("seq:{}", v), value: Some(v as f64), unit: "seq".into(), diagnostic: None }
    }
    pub fn count(v: usize) -> Self {
        Quantity { raw: format!("{}", v), value: Some(v as f64), unit: "count".into(), diagnostic: None }
    }
    pub fn unconvertible(raw: &str, unit: &str, diag: &str) -> Self {
        Quantity { raw: raw.to_string(), value: None, unit: unit.to_string(), diagnostic: Some(diag.to_string()) }
    }
}

/// 一条 LSM 记录（WAL 条目 / memtable 条目 / SSTable 条目共用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub ikey: InternalKey,
    /// point value；删除记录为 None
    pub value: Option<String>,
    /// 范围删除的终点（含/不含由 end_inclusive 决定）
    pub range_end: Option<String>,
    pub end_inclusive: bool,
    /// 原始来源片段（WAL 行 / flush 摘要），导入时保留
    pub source: String,
}

impl Record {
    pub fn put(key: &str, seq: u64, value: &str) -> Self {
        Record { ikey: InternalKey::new(key, seq, KIND_POINT), value: Some(value.into()), range_end: None, end_inclusive: false, source: String::new() }
    }
    pub fn del(key: &str, seq: u64) -> Self {
        Record { ikey: InternalKey::new(key, seq, KIND_DELETE), value: None, range_end: None, end_inclusive: false, source: String::new() }
    }
    pub fn range_del(start: &str, end: &str, seq: u64, end_inclusive: bool) -> Self {
        Record { ikey: InternalKey::new(start, seq, KIND_RANGE_DELETE), value: None, range_end: Some(end.into()), end_inclusive, source: String::new() }
    }
    pub fn is_range_del(&self) -> bool {
        self.ikey.kind == KIND_RANGE_DELETE
    }
    pub fn covers(&self, user_key: &str) -> bool {
        if !self.is_range_del() { return false; }
        let end = self.range_end.as_deref().unwrap_or("");
        user_key >= self.ikey.user_key.as_str()
            && (if self.end_inclusive { user_key <= end } else { user_key < end })
    }
}

/// SSTable 摘要：key range、level、记录、原始表示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SstFile {
    /// 稳定身份（导入/导出往返后保持不变）
    pub id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    pub records: Vec<Record>,
    /// 文件大小（带单位）
    pub size: Quantity,
    /// 原始来源表示（如 "MANIFEST-000012"）
    pub source: String,
}

impl SstFile {
    pub fn overlaps(&self, other: &SstFile) -> bool {
        self.smallest <= other.largest && other.smallest <= self.largest
    }
}

/// 快照：读视图固定在某个序号。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
    pub active: bool,
    pub label: String,
}

/// 压实参数（版本化，修订生成新版本）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionParams {
    pub version: u32,
    /// 触发 L0 压实的文件数阈值
    pub l0_trigger: usize,
    /// 输出文件目标大小（bytes）
    pub target_file_size: u64,
    /// 是否允许范围删除在底层无数据时下沉到底层并丢弃
    pub allow_range_del_elision: bool,
}

impl Default for CompactionParams {
    fn default() -> Self {
        CompactionParams { version: 1, l0_trigger: 4, target_file_size: 2 * 1024 * 1024, allow_range_del_elision: true }
    }
}

/// 层级视图：level -> 文件列表（展示与压实选择用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LevelView {
    pub level: u32,
    pub files: Vec<SstFile>,
}
