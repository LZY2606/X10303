//! 领域材料模型：不可变材料（WAL / memtable / SSTable 摘要 / snapshot / range tombstone）
//! 与可修改决定（压实参数版本、人工决定）分层。

use crate::engine::basis::{KeyRep, Quantity};
use serde::{Deserialize, Serialize};

/// 记录类型。内部键排序类型序：Delete(0) < Put(1)，
/// 即同 (user key, seqno) 时删除先于写入；同序号多类型属于需裁决的情形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Put,
    Delete,
}

impl RecordKind {
    pub fn rank(self) -> u8 {
        match self {
            RecordKind::Delete => 0,
            RecordKind::Put => 1,
        }
    }
}

/// 材料来源（不可变材料与派生结果均记录直接来源）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Provenance {
    /// 直接来源材料稳定 id（如 WAL 条目 id / 源 SSTable id / 压实记录 id）
    pub source_id: String,
    /// 材料版本（不可变材料修订号；输入只改一个直接来源时产生新版本）
    pub material_version: i64,
    /// 人类可读来源说明
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct PointValue {
    /// 导入时的原始表示（永不丢失）
    pub raw_repr: String,
    /// 派生层解码后的字符串表示
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub decoded: Option<String>,
    /// 无法解码时留在原位置的诊断
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PointRecord {
    pub id: String,
    pub key: KeyRep,
    /// 单调内部序列号（携带 seq 单位/基准）
    pub seqno: Quantity,
    pub kind: RecordKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<PointValue>,
    pub origin: Provenance,
    /// 该记录无法被派生层使用时（如无法比较的键），保留记录并附诊断
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

impl PointRecord {
    pub fn seq(&self) -> i64 {
        self.seqno.normalized.unwrap_or(i64::MIN)
    }
    pub fn usable(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RangeTomb {
    pub id: String,
    pub start: KeyRep,
    pub end: KeyRep,
    /// 右端点是否包含。false = [start,end)，true = [start,end]
    pub end_inclusive: bool,
    pub seqno: Quantity,
    pub origin: Provenance,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

impl RangeTomb {
    pub fn seq(&self) -> i64 {
        self.seqno.normalized.unwrap_or(i64::MIN)
    }
    /// 半开/闭区间统一判定：键 k 是否被该范围删除覆盖。
    pub fn covers(&self, k: &KeyRep) -> bool {
        let ord_s = KeyRep::byte_cmp(&k.bytes(), &self.start.bytes());
        let ord_e = KeyRep::byte_cmp(&k.bytes(), &self.end.bytes());
        ord_s != std::cmp::Ordering::Less
            && if self.end_inclusive {
                ord_e != std::cmp::Ordering::Greater
            } else {
                ord_e == std::cmp::Ordering::Less
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seqno: Quantity,
    /// 是否仍活跃（active old snapshot 阻止旧版本下沉丢弃）
    pub active: bool,
    pub origin: Provenance,
}

/// SSTable 摘要。L0 允许互相重叠；L>=1 同层文件键范围必须不重叠。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SSTable {
    pub id: String,
    pub level: Quantity,
    pub smallest: KeyRep,
    pub largest: KeyRep,
    pub points: Vec<PointRecord>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ranges: Vec<RangeTomb>,
    /// 估算大小（携带 byte 单位；无法转换时保留原值 + 诊断）
    pub size_bytes: Quantity,
    pub origin: Provenance,
    /// 材料诊断：无法转换/不可用但不删除记录
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

impl SSTable {
    pub fn level(&self) -> i32 {
        self.level.normalized.unwrap_or(-1) as i32
    }
    pub fn overlaps(&self, other: &SSTable) -> bool {
        let a = (self.smallest.bytes(), self.largest.bytes());
        let b = (other.smallest.bytes(), other.largest.bytes());
        // 闭区间 [smallest,largest] 重叠
        !(a.1 < b.0 || b.1 < a.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WalEntry {
    pub id: String,
    pub seqno: Quantity,
    pub kind: RecordKind,
    pub key: KeyRep,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub start: Option<KeyRep>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end: Option<KeyRep>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_inclusive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<PointValue>,
    /// 是否已应用进 memtable
    pub applied: bool,
    pub origin: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Memtable {
    pub id: String,
    pub points: Vec<PointRecord>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ranges: Vec<RangeTomb>,
    /// flush 后产生的 L0 SSTable id
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub flushed_to_sst: Option<String>,
    pub origin: Provenance,
}

/// 同序号多类型裁决规则（可修改决定，随参数版本演进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SameSeqPolicy {
    /// Delete 先于 Put（内部键类型序），删除胜
    DeleteWins,
    /// 人工策略：Put 胜
    PutWins,
    /// 不裁决：语义无解，保留全部合法候选
    Ambiguous,
}

/// 压实参数（每个版本不可变；参数修订产生新版本与事件）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompactionParams {
    pub version: i64,
    pub same_seq_policy: SameSeqPolicy,
    /// 每个输出 SSTable 最大记录条数预算
    pub max_output_records: i64,
    /// 每个输出 SSTable 最大字节预算；None 表示只有条数预算
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_output_bytes: Option<i64>,
    /// 无法唯一决定时保留的合法候选上限（超出 => 资源预算耗尽）
    pub max_candidates: i64,
    /// 修订理由（写入事件）
    pub change_reason: String,
}

/// 故障注入点（压实中断半写恢复）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FaultPoint {
    /// 不注入
    #[default]
    None,
    /// 输出已物化、manifest 尚未安装时“崩溃”
    CrashBeforeManifest,
}

/// 压实请求（审阅者发起；结果记录输入摘要、参数版本与父级依赖）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompactionRequest {
    pub id: String,
    pub params_version: i64,
    pub input_sst_ids: Vec<String>,
    /// 期望输出层；None 表示按输入层推导（L0->L1, Ln->Ln+1）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_level: Option<i32>,
    #[serde(default)]
    pub fault: FaultPoint,
    pub note: String,
}

/// 内部键比较：user key 字节序升序；seqno 降序；类型序 Delete 先于 Put。
pub fn cmp_internal(
    ka: &[u8],
    sa: i64,
    ta: RecordKind,
    kb: &[u8],
    sb: i64,
    tb: RecordKind,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match KeyRep::byte_cmp(ka, kb) {
        Ordering::Equal => match sb.cmp(&sa) {
            Ordering::Equal => ta.rank().cmp(&tb.rank()),
            o => o,
        },
        o => o,
    }
}

/// 是否构成“同序号多类型”（同 user key、同 seqno、不同类型）。
pub fn same_seq_conflict(a: &PointRecord, b: &PointRecord) -> bool {
    a.key.bytes() == b.key.bytes() && a.seq() == b.seq() && a.kind != b.kind
}
