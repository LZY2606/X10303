//! 领域模型：内部键 / point / range tombstone / 不可变材料 / 规则版本。
//!
//! 约定：
//! - 所有导入值都保留原始表示（`*_raw` + 编码/单位/时间基准），派生层才规范化；
//! - 无法转换的值留在原位置并附 `diagnosis`，绝不为展示而丢弃；
//! - 输入缺必填字段 -> 带位置的 FormatCorrupt 诊断，不用空值/默认值伪装成功。

use crate::domain::error::{DResult, Diagnostic, DomainError};
use serde::{Deserialize, Serialize};

pub type Seq = u64;

/// 键的坐标基准：utf8 或 hex（导入时保留原始表示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEncoding {
    Utf8,
    Hex,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeySpec {
    pub raw: String,
    #[serde(default = "default_key_encoding")]
    pub encoding: KeyEncoding,
}
fn default_key_encoding() -> KeyEncoding {
    KeyEncoding::Utf8
}

impl KeySpec {
    pub fn new(raw: impl Into<String>, encoding: KeyEncoding) -> Self {
        Self { raw: raw.into(), encoding }
    }
    pub fn decode(&self, location: &str) -> DResult<Vec<u8>> {
        match self.encoding {
            KeyEncoding::Utf8 => Ok(self.raw.as_bytes().to_vec()),
            KeyEncoding::Hex => match hex::decode(self.raw.trim()) {
                Ok(b) => Ok(b),
                Err(e) => Err(DomainError::FormatCorrupt {
                    location: location.to_string(),
                    reason: format!("hex 键解码失败: {e}"),
                    raw: self.raw.clone(),
                }),
            },
        }
    }
}

/// 带单位的量：导入保留 `raw`+`unit`，派生层规范化为字节；转不了就留下并附诊断。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Measure {
    pub raw: String,
    /// 单位，例如 `B`、`KiB`、`MiB`。
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<String>,
}

impl Measure {
    pub fn parse(raw: impl Into<String>, unit: impl Into<String>, location: &str) -> Self {
        let raw = raw.into();
        let unit = unit.into();
        match normalize_bytes(&raw, &unit) {
            Ok(b) => Measure { raw, unit, normalized_bytes: Some(b), diagnosis: None },
            Err(d) => Measure { raw, unit, normalized_bytes: None, diagnosis: Some(format!("{d} @ {location}")) },
        }
    }
}

fn normalize_bytes(raw: &str, unit: &str) -> Result<u64, String> {
    let n: u64 = raw.trim().parse().map_err(|_| format!("无法把 {raw:?} 解析为字节数"))?;
    let mult: u64 = match unit.trim() {
        "B" | "bytes" => 1,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        other => return Err(format!("未知字节单位 {other:?}")),
    };
    n.checked_mul(mult).ok_or_else(|| "字节数溢出".to_string())
}

/// 时间坐标基准（epoch_ms / 墙钟本地），派生层给出归一化毫秒。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimePoint {
    pub raw: String,
    /// 坐标基准：`epoch_ms`（Unix 毫秒）或 `wall_local`（本地墙钟字符串）。
    pub basis: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_epoch_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<String>,
}

impl TimePoint {
    pub fn epoch_ms(raw: i64) -> Self {
        TimePoint {
            raw: raw.to_string(),
            basis: "epoch_ms".to_string(),
            normalized_epoch_ms: Some(raw),
            diagnosis: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Put,
    Delete,
}

/// 值描述：保留原始表示与编码，派生层给出字节数（带单位）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValueSpec {
    pub raw: String,
    #[serde(default = "default_key_encoding")]
    pub encoding: KeyEncoding,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<Measure>,
}

impl ValueSpec {
    pub fn utf8(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let size = Measure::parse(raw.len().to_string(), "B", "value.size");
        ValueSpec { raw, encoding: KeyEncoding::Utf8, size: Some(size) }
    }
    pub fn decode(&self, location: &str) -> DResult<Vec<u8>> {
        match self.encoding {
            KeyEncoding::Utf8 => Ok(self.raw.as_bytes().to_vec()),
            KeyEncoding::Hex => hex::decode(self.raw.trim()).map_err(|e| DomainError::FormatCorrupt {
                location: location.to_string(),
                reason: format!("hex 值解码失败: {e}"),
                raw: self.raw.clone(),
            }),
        }
    }
}

/// 直接来源：稳定材料身份 + 材料内位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OriginRef {
    pub material_id: String,
    /// 材料内 JSON 指针式位置，如 `/files/0/records/3`。
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PointRecord {
    /// 记录稳定身份（在其材料内按内容与位置确定）。
    pub rid: String,
    pub seq: Seq,
    pub kind: PointKind,
    pub key: KeySpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<ValueSpec>,
    pub origin: OriginRef,
}

/// 范围删除端点：start 恒为闭区间；end 支持开/闭端点（边界情形显式化）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RangeTombstone {
    pub rid: String,
    pub seq: Seq,
    pub start: KeySpec,
    pub end: KeySpec,
    /// true => [start, end]；false => [start, end)。
    pub end_inclusive: bool,
    pub origin: OriginRef,
}

impl RangeTombstone {
    pub fn covers(&self, key: &[u8], loc: &str) -> DResult<bool> {
        let s = self.start.decode(loc)?;
        let e = self.end.decode(loc)?;
        Ok(key >= s.as_slice() && if self.end_inclusive { key <= e.as_slice() } else { key < e.as_slice() })
    }
    /// 与键区间 [lo, hi] 是否重叠（两端点按各自开闭语义）。
    pub fn overlaps(&self, lo: &[u8], hi: &[u8], loc: &str) -> DResult<bool> {
        let s = self.start.decode(loc)?;
        let e = self.end.decode(loc)?;
        let real_hi = if self.end_inclusive { e } else { e.clone() };
        // 离散比较：区间 R 与 [lo,hi] 不重叠 <=> R.end < lo（闭区间）或 R.end <= lo（开区间）
        let before = if self.end_inclusive { real_hi.as_slice() < lo } else { e.as_slice() <= lo };
        let after = s.as_slice() > hi;
        Ok(!before && !after)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SstFileSpec {
    /// 稳定逻辑身份（导入材料给出，如 `L0-a`），导出再导入不变。
    pub cid: String,
    pub level: u32,
    pub smallest: KeySpec,
    pub largest: KeySpec,
    pub size: Measure,
    #[serde(default)]
    pub records: Vec<PointRecord>,
    #[serde(default)]
    pub range_deletes: Vec<RangeTombstone>,
    pub origin: OriginRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalEntry {
    pub index: u32,
    pub seq: Seq,
    pub op: String, // put / delete / delete_range
    pub key: KeySpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<KeySpec>,
    #[serde(default)]
    pub end_inclusive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<ValueSpec>,
    pub at: TimePoint,
    pub origin: OriginRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalMaterial {
    pub wal_id: String,
    pub entries: Vec<WalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlushMaterial {
    pub flush_id: String,
    pub wal_id: String,
    pub seq_lo: Seq,
    pub seq_hi: Seq,
    pub result_sst_cid: String,
    pub flushed_at: TimePoint,
    pub entry_refs: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SstablesMaterial {
    pub layer: String,
    pub files: Vec<SstFileSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotSpec {
    pub snap_id: String,
    pub seq: Seq,
    pub active: bool,
    pub created: TimePoint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotsMaterial {
    pub snapshots: Vec<SnapshotSpec>,
}

/// 规则/参数的一个不可变版本（“参数修订”通过激活新版本体现，旧版本永不被覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleSet {
    pub version: String,
    /// 单个输出文件大小上限（原始表示 + 单位）。
    pub max_output_size: Measure,
    /// 是否在压实时自动扩展重叠 L0。
    pub expand_overlapping_l0: bool,
    /// 一次压实允许拉入的最大文件数（资源预算）。
    pub max_input_files: u32,
    pub notes: String,
}

/// 不可变材料的统一信封。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Material {
    Wal(WalMaterial),
    Flush(FlushMaterial),
    Sstables(SstablesMaterial),
    Snapshots(SnapshotsMaterial),
    Rules(RuleSet),
}

impl Material {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Material::Wal(_) => "wal",
            Material::Flush(_) => "flush",
            Material::Sstables(_) => "sstables",
            Material::Snapshots(_) => "snapshots",
            Material::Rules(_) => "rules",
        }
    }
}

/// 导入报告：整体失败（FormatCorrupt）与逐条诊断分层。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportReport {
    pub accepted: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}
