//! 坐标基准 / 单位表示。
//! 所有领域量都携带单位或坐标基准；导入时保留原始表示，只有派生层做规范化。
//! 无法转换的值留在原位置并附诊断，绝不为了画图而丢弃。

use serde::{Deserialize, Serialize};

/// 坐标/单位基准的稳定标识（内置 7 个基准，导入数据只允许引用或原样声明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BasisKind {
    /// 单调逻辑序列号（internal sequence number），单位“序号”
    SequenceNumber,
    /// 逻辑时钟（事件序号），单位“事件”
    LogicalClock,
    /// 墙钟时间，坐标基准 UTC，单位 ms
    WallClockUtcMs,
    /// 层级坐标，单位“层号（L0 可重叠，L>=1 不重叠）”
    Level,
    /// 字节长度，单位 byte
    ByteSize,
    /// 记录条数，单位“条”
    RecordCount,
    /// 字节序键比较，坐标基准“无符号字节字典序”
    UnsignedByteLex,
}

impl BasisKind {
    pub fn id(self) -> &'static str {
        match self {
            BasisKind::SequenceNumber => "basis.seqno",
            BasisKind::LogicalClock => "basis.logical_clock",
            BasisKind::WallClockUtcMs => "basis.wall_clock_utc_ms",
            BasisKind::Level => "basis.level",
            BasisKind::ByteSize => "basis.byte_size",
            BasisKind::RecordCount => "basis.record_count",
            BasisKind::UnsignedByteLex => "basis.byte_lex",
        }
    }
    pub fn unit(self) -> &'static str {
        match self {
            BasisKind::SequenceNumber => "seq",
            BasisKind::LogicalClock => "event",
            BasisKind::WallClockUtcMs => "ms(UTC)",
            BasisKind::Level => "level",
            BasisKind::ByteSize => "byte",
            BasisKind::RecordCount => "record",
            BasisKind::UnsignedByteLex => "byte-lex",
        }
    }
}

/// 基准引用：id 稳定，kind 决定规范化方式。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BasisId {
    pub id: String,
    pub kind: BasisKind,
}

impl BasisId {
    pub fn builtin(kind: BasisKind) -> Self {
        BasisId { id: kind.id().to_string(), kind }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnitKind {
    Seq,
    Event,
    WallClockUtcMs,
    Level,
    Byte,
    Record,
    ByteLex,
    /// 原始导入单位，派生层无法规范化时保留
    Raw,
}

/// 带单位/坐标基准的量。保留 `raw_repr` 原始表示；`normalized` 只在派生层计算。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Quantity {
    /// 导入时的原始文本表示（永不丢失）
    pub raw_repr: String,
    pub unit: UnitKind,
    pub basis: BasisId,
    /// 派生层规范化结果；None 表示无法转换，此时必须有诊断
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub normalized: Option<i64>,
    /// 无法规范化的原因代码（留在原位置的诊断）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub diagnostic: Option<String>,
}

impl Quantity {
    pub fn seq(n: i64) -> Self {
        Quantity {
            raw_repr: n.to_string(),
            unit: UnitKind::Seq,
            basis: BasisId::builtin(BasisKind::SequenceNumber),
            normalized: Some(n),
            diagnostic: None,
        }
    }
    pub fn event(n: i64) -> Self {
        Quantity {
            raw_repr: n.to_string(),
            unit: UnitKind::Event,
            basis: BasisId::builtin(BasisKind::LogicalClock),
            normalized: Some(n),
            diagnostic: None,
        }
    }
    pub fn level(n: i32) -> Self {
        Quantity {
            raw_repr: format!("L{n}"),
            unit: UnitKind::Level,
            basis: BasisId::builtin(BasisKind::Level),
            normalized: Some(n as i64),
            diagnostic: None,
        }
    }
    pub fn bytes(n: Option<i64>, raw_repr: impl Into<String>) -> Self {
        Quantity {
            raw_repr: raw_repr.into(),
            unit: UnitKind::Byte,
            basis: BasisId::builtin(BasisKind::ByteSize),
            normalized: n,
            diagnostic: if n.is_none() { Some("D_UNCONVERTIBLE_SIZE".into()) } else { None },
        }
    }
    pub fn records(n: i64) -> Self {
        Quantity {
            raw_repr: n.to_string(),
            unit: UnitKind::Record,
            basis: BasisId::builtin(BasisKind::RecordCount),
            normalized: Some(n),
            diagnostic: None,
        }
    }
    /// 无法转换的原始量：保留原值 + 诊断
    pub fn unconvertible(raw_repr: impl Into<String>, unit: &str, reason: impl Into<String>) -> Self {
        Quantity {
            raw_repr: raw_repr.into(),
            unit: UnitKind::Raw,
            basis: BasisId { id: format!("basis.raw:{unit}"), kind: BasisKind::ByteSize },
            normalized: None,
            diagnostic: Some(reason.into()),
        }
    }
}

/// 内部键表示。导入保留原始编码；键比较只依据声明的字节序基准。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyRep {
    /// 逻辑用户键（本案例包统一用可读逻辑名，如 "k03"）
    pub logical: String,
    /// 原始编码表示（十六进制），导入时保留
    pub encoded_hex: String,
    pub basis: BasisId,
}

impl KeyRep {
    pub fn new(logical: impl Into<String>) -> Self {
        let logical = logical.into();
        let encoded_hex = hex_encode(logical.as_bytes());
        KeyRep { logical, encoded_hex, basis: BasisId::builtin(BasisKind::UnsignedByteLex) }
    }
    /// 无符号字节字典序比较坐标基准下的字节序列。
    pub fn byte_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        a.cmp(b)
    }
    pub fn bytes(&self) -> Vec<u8> {
        hex_decode(&self.encoded_hex).unwrap_or_else(|| self.logical.as_bytes().to_vec())
    }
}

pub fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let h = (b[i] as char).to_digit(16)?;
        let l = (b[i + 1] as char).to_digit(16)?;
        out.push((h * 16 + l) as u8);
        i += 2;
    }
    Some(out)
}
