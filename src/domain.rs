//! 领域模型：sequence number、snapshot、SSTable、point/range tombstone 等。
//!
//! 所有领域量都携带单位/坐标基准；导入材料保留原始表示(`raw`)，
//! 只有派生层才填充 `normalized`。无法转换时 `normalized = None` 并附诊断。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// 规则版本：读取顺序、可见性、压实下沉判定全部归属此版本。
pub const RULE_VERSION: &str = "lsm-rules:v1";
/// key 的坐标基准：UTF-8 字节序字典序。
pub const KEY_BASIS: &str = "keyspace:utf8-lex:v1";
/// sequence number 的单位基准。
pub const SEQ_BASIS: &str = "seq:monotonic:u64";
/// 时间基准。
pub const TIME_BASIS: &str = "time:unix-ns:u64";
/// 字节单位。
pub const UNIT_BYTE: &str = "byte";

/// 带单位的量：`raw` 是导入时的原始表示，绝不丢弃；`normalized` 仅派生层填写。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RawQty {
    pub raw: String,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl RawQty {
    pub fn new(raw: impl Into<String>, unit: impl Into<String>) -> Self {
        RawQty { raw: raw.into(), unit: unit.into(), normalized: None, note: None }
    }
    pub fn bytes(n: u64) -> Self {
        RawQty { raw: n.to_string(), unit: UNIT_BYTE.to_string(), normalized: Some(n), note: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Put,
    Delete,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Inclusive,
    Exclusive,
}

/// 同序号不同类型冲突时的裁决规则（参数版本可修订）。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TiePolicy {
    /// 删除优先：RangeDelete < PointDelete < Put（内部键序中越前越新）。
    DeleteWins,
    /// 写入优先：Put < PointDelete < RangeDelete。
    PutWins,
    /// 不裁决：返回 SemanticUnsolvable，并保留全部合法候选顺序。
    RejectDuplicate,
}

/// point 操作（Put 值或 point tombstone）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PointOp {
    pub id: String,
    pub user_key: String,
    pub seq: u64,
    pub kind: PointKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<RawQty>,
    /// 直接来源材料 id（WAL entry 或派生片段）。
    pub origin: String,
}

/// range tombstone（删除记录），端点语义显式保存。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RangeTombstone {
    pub id: String,
    pub start_key: String,
    pub start_kind: EndpointKind,
    pub end_key: String,
    pub end_kind: EndpointKind,
    pub seq: u64,
    pub origin: String,
    /// 若由输出切分裁剪而来，记录父 range tombstone id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment_of: Option<String>,
}

impl RangeTombstone {
    pub fn covers(&self, key: &str) -> bool {
        let left = match self.start_kind {
            EndpointKind::Inclusive => key >= self.start_key.as_str(),
            EndpointKind::Exclusive => key > self.start_key.as_str(),
        };
        let right = match self.end_kind {
            EndpointKind::Inclusive => key <= self.end_key.as_str(),
            EndpointKind::Exclusive => key < self.end_key.as_str(),
        };
        left && right
    }
    pub fn intersects_file(&self, f: &SstFile) -> bool {
        let a = match self.start_kind {
            EndpointKind::Inclusive => self.start_key.as_str() <= f.max_key.as_str(),
            EndpointKind::Exclusive => self.start_key.as_str() < f.max_key.as_str(),
        };
        let b = match self.end_kind {
            EndpointKind::Inclusive => self.end_key.as_str() >= f.min_key.as_str(),
            EndpointKind::Exclusive => self.end_key.as_str() > f.min_key.as_str(),
        };
        a && b
    }
}

/// WAL 条目（不可变导入材料，保留原始行表示）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WalEntry {
    pub id: String,
    pub seq: u64,
    pub op: WalOp,
    pub at_basis: String,
    /// 导入时的原始文本行，不参与规范化、仅为保真保存。
    pub raw: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum WalOp {
    Put { key: String, value: RawQty },
    Delete { key: String },
    RangeDelete {
        start_key: String,
        start_kind: EndpointKind,
        end_key: String,
        end_kind: EndpointKind,
    },
}

/// 读快照。`active=false` 表示已释放，不再约束墓碑下沉。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
    pub active: bool,
    pub created_basis: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum FileOrigin {
    Flush { flush_id: String },
    Compaction { result_id: String, index: usize },
    Imported,
}

/// SSTable 文件摘要（多层）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SstFile {
    pub id: String,
    pub level: i32,
    pub min_key: String,
    pub max_key: String,
    pub min_seq: u64,
    pub max_seq: u64,
    pub size: RawQty,
    pub points: Vec<PointOp>,
    pub ranges: Vec<RangeTombstone>,
    pub origin: FileOrigin,
    pub key_basis: String,
}

impl SstFile {
    pub fn overlaps_key_range(&self, min_key: &str, max_key: &str) -> bool {
        self.min_key.as_str() <= max_key && self.max_key.as_str() >= min_key
    }
    pub fn contains_key(&self, key: &str) -> bool {
        key >= self.min_key.as_str() && key <= self.max_key.as_str()
    }
}

/// 参数版本（可修改决定层；修订只新增版本，绝不覆盖旧版本）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ParamsV {
    pub version: String,
    pub tie_policy: TiePolicy,
    pub max_output_bytes: RawQty,
    pub max_total_input_bytes: RawQty,
    pub key_basis: String,
    pub seq_basis: String,
    pub note: String,
}

/// 带位置的诊断（输入不完整/损坏/不可换算时使用，绝不用空值伪装成功）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub id: String,
    pub code: String,
    pub severity: String,
    /// 例：`bundle.materials.wal[2].value.raw`
    pub position: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_kept: Option<String>,
}

/// 内部键序：user_key 升序；同 key 按 seq 降序；同 key 同 seq 由 tie policy 决定类型次序。
/// 返回 Ordering 表示 `a` 相对 `b` 的新旧（Less = a 在内部键序中更靠前=更新）。
pub fn internal_type_rank(kind: PointKind, policy: TiePolicy) -> u8 {
    match (kind, policy) {
        (PointKind::Delete, TiePolicy::DeleteWins) => 0,
        (PointKind::Put, TiePolicy::DeleteWins) => 1,
        (PointKind::Put, TiePolicy::PutWins) => 0,
        (PointKind::Delete, TiePolicy::PutWins) => 1,
        (_, TiePolicy::RejectDuplicate) => 2,
    }
}

/// range tombstone 在同序号比较中的类型秩（0=最新）。
pub fn range_type_rank(policy: TiePolicy) -> u8 {
    match policy {
        TiePolicy::DeleteWins => 0,
        TiePolicy::PutWins => 2,
        TiePolicy::RejectDuplicate => 2,
    }
}

pub fn compare_points(a: &PointOp, b: &PointOp, policy: TiePolicy) -> Ordering {
    match a.user_key.cmp(&b.user_key) {
        Ordering::Equal => match b.seq.cmp(&a.seq) {
            Ordering::Equal => {
                internal_type_rank(a.kind, policy).cmp(&internal_type_rank(b.kind, policy))
            }
            o => o,
        },
        o => o,
    }
}

/// 比较 point 与覆盖它的 range tombstone：Less 表示 range 比 point 更新（point 被遮蔽）。
pub fn compare_range_vs_point(r: &RangeTombstone, p: &PointOp, policy: TiePolicy) -> Ordering {
    match r.seq.cmp(&p.seq) {
        Ordering::Equal => range_type_rank(policy).cmp(&internal_type_rank(p.kind, policy)),
        Ordering::Greater => Ordering::Less,
        Ordering::Less => Ordering::Greater,
    }
}
