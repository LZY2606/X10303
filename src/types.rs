//! 领域核心类型：不可变材料（WAL / memtable flush / 多层 SSTable 摘要）
//! 与压实派生物。所有领域量都携带单位或坐标基准：
//! - key 一律使用十六进制（hex）字节表示，坐标基准是“字节序字典序”。
//! - SSTable 的 key range 使用闭区间 [start_hex, end_hex]（inclusive），basis 显式记录。
//! - 范围删除（range tombstone）使用半开区间 [start, end)（end-exclusive），basis 显式记录。
//! - 长度/大小字段携带 `unit`（默认 "bytes"），seq 携带 `basis = "sequence-number"`。
//!
//! 导入时保留原始表示（Raw），派生层才做规范化；无法转换的值留在原位置并附诊断。

use serde::{Deserialize, Serialize};

pub type Seq = u64;

/// 内部键类型序。同一 (user_key, seq) 出现多种类型属于“同序号多类型”边界：
/// 排列顺序固定为 Put > Delete（更大类型序排前，作为内部键序的合法但有争议的打破者），
/// range delete 不与点条目共享同一 user_key,seq 的点序比较（按其自身规则覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Put,
    Delete,
    RangeDelete,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Put => "put",
            EntryKind::Delete => "delete",
            EntryKind::RangeDelete => "range_delete",
        }
    }
    pub fn parse(s: &str) -> Option<EntryKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "put" | "set" => Some(EntryKind::Put),
            "delete" | "del" | "point_delete" => Some(EntryKind::Delete),
            "range_delete" | "rangedel" | "range" => Some(EntryKind::RangeDelete),
            _ => None,
        }
    }
    /// 内部键序用：序号相同时，类型序大的排前。
    pub fn rank(self) -> u8 {
        match self {
            EntryKind::Put => 2,
            EntryKind::Delete => 1,
            EntryKind::RangeDelete => 0,
        }
    }
}

/// 原始表示：导入时逐字保留，便于“不能为了画图而丢掉”。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "repr")]
pub enum RawRepr {
    /// 十六进制字节串（规范坐标）。
    Hex { raw: String },
    /// 带引号文本，按 UTF-8 规范化为字节（派生层）。
    Text { raw: String },
    /// 原始 token 无法转换为字节：保留在原位置并附诊断。
    Unconvertible { raw: String },
}

impl RawRepr {
    pub fn raw(&self) -> &str {
        match self {
            RawRepr::Hex { raw } | RawRepr::Text { raw } | RawRepr::Unconvertible { raw } => raw,
        }
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            _ => None,
        }
    }
}

/// 领域字面量：原始表示 + 派生规范化字节 + 诊断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Literal {
    pub raw: RawRepr,
    /// 派生层规范化结果（hex 解码或文本编码）。无法转换时为 None，值留在原位。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_hex: Option<String>,
    /// 该值携带的单位或坐标基准。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// 无法转换/可疑表示的诊断（保留值但不假装成功）。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diag: Vec<String>,
}

/// 来源坐标：每条材料都能反查到文件、行、列与原始行文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    pub source: String,
    pub line: usize,
    pub col: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_text: Option<String>,
}

impl std::fmt::Display for Pos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.source, self.line, self.col)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosDiag {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pos: Option<Pos>,
}

impl PosDiag {
    pub fn new(code: impl Into<String>, message: impl Into<String>, pos: Option<Pos>) -> Self {
        Self { code: code.into(), message: message.into(), pos }
    }
}

/// 点条目（WAL put/delete 或 SST 内部条目）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointEntry {
    /// 稳定身份：`pt:<sha8(origin.id|user_key_hex|seq|kind)>`。
    pub id: String,
    pub user_key_hex: String,
    pub key: Literal,
    pub seq: Seq,
    pub seq_basis: String,
    pub kind: EntryKind,
    /// put 的载荷；delete 为 None（语义删除，不是空值伪装）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Literal>,
    pub origin: Origin,
    pub pos: Pos,
}

/// 范围删除（range tombstone）：半开区间 [start, end)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeTombstone {
    /// 稳定身份：`rt:<sha8(origin.id|start|end|seq)>`。
    pub id: String,
    pub start_hex: String,
    pub end_hex: String,
    pub interval_basis: String,
    pub seq: Seq,
    pub seq_basis: String,
    pub origin: Origin,
    pub pos: Pos,
}

/// 条目出处：位于哪个 WAL 或哪个 SST，以及该文件内序号（保持来源关系）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// 容器稳定 id（wal 或 sst）。
    pub file_id: String,
    /// 容器内条目序号（0 基），用于稳定排序与来源关系。
    pub index: usize,
    /// 若由 flush 从 WAL 派生，则指向来源 WAL 文件 id。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_flush: Option<String>,
}

/// 一个 SSTable 摘要。key range 为闭区间，覆盖所有内部条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SstFile {
    pub id: String,
    pub level: i32,
    pub level_basis: String,
    pub start_hex: String,
    pub end_hex: String,
    pub range_basis: String,
    /// 文件大小估算（默认 bytes）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub size_unit: String,
    pub points: Vec<PointEntry>,
    pub ranges: Vec<RangeTombstone>,
    /// 该文件如何产生：imported（直接材料）/ flush / compaction（派生，含父依赖）。
    pub provenance: FileProvenance,
    pub pos: Pos,
    /// range/大小等缺失或派生时的诊断。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diag: Vec<PosDiag>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FileProvenance {
    Imported,
    Flush { wal_id: String, flush_id: String },
    Compaction {
        comp_id: String,
        event_id: String,
        /// 父级依赖（被合并的输入 SST 稳定 id）。
        inputs: Vec<String>,
        params_version: String,
    },
}

/// 一个 WAL（memtable 尚未 flush 的写入日志）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalFile {
    pub id: String,
    pub points: Vec<PointEntry>,
    pub ranges: Vec<RangeTombstone>,
    pub pos: Pos,
}

/// memtable flush：把一个 WAL 物化为指定层级（通常 L0）的 SST。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flush {
    pub id: String,
    pub wal_id: String,
    pub sst_id: String,
    pub to_level: i32,
    pub pos: Pos,
}

/// 读快照。open_seq 是获取快照时的写入序列号（仅可见 seq <= open_seq）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub open_seq: Seq,
    pub seq_basis: String,
    pub open_event: String,
    /// None 表示仍然活跃；Some(event_id) 表示在该事件处关闭（仍保留为历史）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_event: Option<String>,
    pub pos: Pos,
}
