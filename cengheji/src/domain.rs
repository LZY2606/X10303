//! 领域模型：内部键、snapshot、SSTable 摘要、point/range tombstone、带单位的量。
//!
//! 约定（坐标基准）：
//! - `seq` 为全局单调序列号，原点 0 保留给“无数据”，业务记录从 1 开始；WAL 另有 `lsn` 偏移坐标。
//! - 用户键为字节串，字节字典序即内部键排序的第一分量。
//! - 内部键顺序 = (user_key 升序, seq 降序, kind 降序: Delete > Set)。
//! - key range 一律半开 [start, end)；端点保留原始表示（utf8/hex），派生层才转字节比较。
//! - 所有物理量携带单位；无法转换的值原地保留并附诊断，不用默认值顶替。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub const SEQ_ORIGIN: u64 = 0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub severity: DiagSeverity,
    pub location: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagSeverity {
    Error,
    Warning,
    Info,
}

/// 带单位的物理量。`raw` 是导入时的原始表示，规范化只在派生层发生。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Measured {
    pub raw: String,
    pub unit: String,
    pub magnitude: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

impl Measured {
    pub fn bytes(raw: &str, magnitude: i64) -> Self {
        Measured { raw: raw.to_string(), unit: "byte".into(), magnitude: Some(magnitude), diagnostics: vec![] }
    }
    pub fn magnitude_or(&self) -> Result<i64, Diagnostic> {
        self.magnitude.ok_or_else(|| Diagnostic {
            code: "unconvertible_quantity".into(),
            severity: DiagSeverity::Error,
            location: format!("measured[{}]", self.raw),
            message: format!("数值 {:?} 无法转换为单位 {}，保留原值", self.raw, self.unit),
            candidates: vec![],
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Utf8,
    Hex,
}

impl Encoding {
    pub fn decode(&self, raw: &str) -> Result<Vec<u8>, Diagnostic> {
        match self {
            Encoding::Utf8 => Ok(raw.as_bytes().to_vec()),
            Encoding::Hex => hex_decode(raw).map_err(|e| Diagnostic {
                code: "corrupt_endpoint_hex".into(),
                severity: DiagSeverity::Error,
                location: format!("endpoint[{}]", raw),
                message: e,
                candidates: vec![],
            }),
        }
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return Err("hex 长度必须为偶数".into());
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16).ok_or_else(|| format!("非法 hex 字符: {}", b[i] as char))?;
        let lo = (b[i + 1] as char).to_digit(16).ok_or_else(|| format!("非法 hex 字符: {}", b[i + 1] as char))?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Ok(out)
}

/// 键/范围端点：原始表示永不丢失。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Endpoint {
    pub raw: String,
    pub encoding: Encoding,
}

impl Endpoint {
    pub fn utf8(raw: &str) -> Self {
        Endpoint { raw: raw.to_string(), encoding: Encoding::Utf8 }
    }
    pub fn bytes(&self) -> Result<Vec<u8>, Diagnostic> {
        self.encoding.decode(&self.raw)
    }
}

/// 半开区间 [start, end)，基准：字节字典序。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyRange {
    pub start: Endpoint,
    pub end: Endpoint,
    #[serde(rename = "intervalBasis")]
    pub interval_basis: String,
}

impl KeyRange {
    pub fn half_open_utf8(start: &str, end: &str) -> Self {
        KeyRange {
            start: Endpoint::utf8(start),
            end: Endpoint::utf8(end),
            interval_basis: "byte-lexicographic [start,end)".into(),
        }
    }
    pub fn decoded(&self) -> Result<DecodedRange, Diagnostic> {
        let s = self.start.bytes()?;
        let e = self.end.bytes()?;
        Ok(DecodedRange { start: s, end: e })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRange {
    pub start: Vec<u8>,
    pub end: Vec<u8>,
}

impl DecodedRange {
    pub fn is_valid_half_open(&self) -> bool {
        self.start < self.end
    }
    /// 半开区间相交（空集不算）。
    pub fn intersects(&self, other: &DecodedRange) -> bool {
        self.start < other.end && other.start < self.end
    }
    pub fn contains(&self, key: &[u8]) -> bool {
        self.start.as_slice() <= key && key < self.end.as_slice()
    }
    /// 与另一区间求交，返回半开区间；无交返回 None。
    pub fn clip(&self, within: &DecodedRange) -> Option<DecodedRange> {
        let s = self.start.clone().max(within.start.clone());
        let e = self.end.clone().min(within.end.clone());
        if s < e { Some(DecodedRange { start: s, end: e }) } else { None }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Set,
    Delete,
}

/// point value：原始表示必保留；normalized 是派生层结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PointValue {
    pub representation: String,
    pub encoding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// 一条 point 记录（内部键的承载者）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PointRecord {
    pub id: String,
    pub seq: u64,
    pub kind: PointKind,
    pub key: Endpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<PointValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// range tombstone：[range.start, range.end) @ seq。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeTomb {
    pub id: String,
    pub seq: u64,
    pub range: KeyRange,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileOrigin {
    MemtableFlush,
    IngestedSummary,
    CompactionOutput,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileState {
    Live,
    Staging,
    Obsolete,
}

/// SSTable 摘要（多层）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SSTable {
    pub id: String,
    pub level: i64,
    pub state: FileState,
    pub origin: FileOrigin,
    pub key_range: KeyRange,
    pub size: Measured,
    #[serde(default)]
    pub points: Vec<PointRecord>,
    #[serde(default)]
    pub ranges: Vec<RangeTomb>,
    /// 溯源：由哪条材料/哪个 run 产生。
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub material_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<String>,
}

impl Provenance {
    pub fn material(ids: &[String]) -> Self {
        Provenance { material_ids: ids.to_vec(), run_id: None, params_version: None, parent_event_ids: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
    pub active: bool,
    pub basis: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WalKind {
    Set,
    Delete,
    RangeDelete,
}

/// WAL 原始记录；同一 seq 允许出现多类型（崩溃恢复重放的真实情形）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalRecord {
    pub id: String,
    pub lsn: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub kind: WalKind,
    pub key: Endpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<PointValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_end: Option<Endpoint>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialKind {
    WalRecord,
    FlushOrder,
    SSTableSummary,
    Snapshot,
}

/// 不可变材料。导入只校验结构；修订以新 id 取代旧 id，旧件永不改写。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawMaterial {
    pub id: String,
    pub kind: MaterialKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    pub representation: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// 内部键比较：user_key 升序、seq 降序、Delete 先于 Set。
pub fn internal_cmp(a_key: &[u8], a_seq: u64, a_kind: PointKind, b: &PointRecord, b_key: &[u8]) -> Ordering {
    match a_key.cmp(b_key) {
        Ordering::Equal => match b.seq.cmp(&a_seq) {
            Ordering::Equal => kind_rank(a_kind).cmp(&kind_rank(b.kind)).reverse(),
            o => o,
        },
        o => o,
    }
}

pub fn kind_rank(k: PointKind) -> u8 {
    match k {
        PointKind::Delete => 1,
        PointKind::Set => 0,
    }
}

/// 稳定 JSON：键名字典序，用于摘要与身份摘要（不是显示格式）。
pub fn canonical_json(v: &Value) -> String {
    let ordered = sort_value(v);
    // to_string 对 BTreeMap 按序输出；Value::Object 来自 Map（BTreeMap 排序）。
    serde_json::to_string(&ordered).unwrap_or_default()
}

fn sort_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<_, _> = m.iter().map(|(k, vv)| (k.clone(), sort_value(vv))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

pub fn digest_of(v: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(canonical_json(v).as_bytes());
    let out = h.finalize();
    hex::encode(&out[..10])
}
