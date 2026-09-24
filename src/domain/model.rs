//! 不可变材料的领域模型：WAL、memtable flush、SSTable 摘要、snapshot、内部键。

use crate::domain::ids::stable_id;
use serde::{Deserialize, Serialize};

pub type Seq = u64;

/// 内部键值类型。同一 user_key + seq 上的排序权重按此降序：
/// RANGE_DEL_END > RANGE_DEL_BEGIN > DELETE > PUT。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpType {
    Put = 0,
    Delete = 1,
    RangeDelBegin = 2,
    RangeDelEnd = 3,
}

impl OpType {
    pub fn label(self) -> &'static str {
        match self {
            OpType::Put => "PUT",
            OpType::Delete => "DELETE",
            OpType::RangeDelBegin => "RANGE_BEGIN",
            OpType::RangeDelEnd => "RANGE_END",
        }
    }

    /// 范围删除两端之外的键是否落在范围内。
    pub fn is_range_marker(self) -> bool {
        matches!(self, OpType::RangeDelBegin | OpType::RangeDelEnd)
    }
}

/// 点记录（PUT / DELETE）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointOp {
    pub key: String,
    pub seq: Seq,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<crate::domain::units::Measured>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Put,
    Delete,
}

impl PointOp {
    pub fn op_type(&self) -> OpType {
        match self.kind {
            PointKind::Put => OpType::Put,
            PointKind::Delete => OpType::Delete,
        }
    }
}

/// 范围删除。半开区间语义按 `end_exclusive` 实现；端点边界在 compaction 中显式记录判定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeDelete {
    pub begin: String,
    pub end_exclusive: String,
    pub seq: Seq,
    pub name: String,
}

impl RangeDelete {
    pub fn contains(&self, key: &str) -> bool {
        key >= self.begin.as_str() && key < self.end_exclusive.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WalRecordData {
    Point(PointOp),
    RangeDelete(RangeDelete),
}

impl WalRecordData {
    pub fn seq(&self) -> Seq {
        match self {
            WalRecordData::Point(p) => p.seq,
            WalRecordData::RangeDelete(r) => r.seq,
        }
    }

    pub fn touched_keys(&self) -> Vec<String> {
        match self {
            WalRecordData::Point(p) => vec![p.key.clone()],
            WalRecordData::RangeDelete(r) => vec![r.begin.clone(), r.end_exclusive.clone()],
        }
    }
}

/// WAL 不可变记录。修订不是原地覆盖，而是追加新版本，旧版本保留并标记 superseded。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalRecord {
    pub id: String,
    pub version: u32,
    pub epoch_ms: u64,
    pub data: WalRecordData,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub revised_from: Option<String>,
}

impl WalRecord {
    pub fn new(version: u32, epoch_ms: u64, data: WalRecordData, revised_from: Option<String>) -> Self {
        let id_seed = serde_json::json!({
            "v": version, "data": data, "revised_from": revised_from,
        });
        Self {
            id: stable_id("wal", &id_seed),
            version,
            epoch_ms,
            data,
            superseded_by: None,
            revised_from,
        }
    }

    pub fn current(&self) -> bool {
        self.superseded_by.is_none()
    }
}

/// SSTable 中携带的内部条目（点）。`entry_id` 是跨重放稳定的版本链节点身份。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub entry_id: String,
    pub key: String,
    pub seq: Seq,
    pub op: OpType,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<crate::domain::units::Measured>,
    /// 来源：wal / lsm / range / synthetic，紧跟材料 id，形成来源关系。
    pub origin: String,
}

pub fn entry_id(origin: &str, key: &str, seq: Seq, op: OpType) -> String {
    stable_id(
        "entry",
        &serde_json::json!({"origin": origin, "key": key, "seq": seq, "op": op}),
    )
}

/// 范围删除在某层文件中的片段（可能因输出切分而被裁剪/复制）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeFragment {
    pub fragment_id: String,
    pub name: String,
    pub seq: Seq,
    pub begin: String,
    pub end_exclusive: String,
    pub origin_wal: String,
    /// 该片段为何保留：旧 snapshot / 未参与压实的旧数据 / 切分复制。
    pub retention: Vec<String>,
}

pub fn fragment_id(name: &str, seq: Seq, begin: &str, end: &str, origin: &str) -> String {
    stable_id(
        "rfrag",
        &serde_json::json!({"name": name, "seq": seq, "begin": begin, "end": end, "origin": origin}),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizeHint {
    pub value: crate::domain::units::Measured,
}

/// SSTable 摘要。所有摘要材料都是不可变的；压实不修改旧文件，而是产生新文件事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SstFile {
    pub id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    pub end_inclusive: bool,
    pub entries: Vec<Entry>,
    pub range_fragments: Vec<RangeFragment>,
    pub size_hint: Option<SizeHint>,
    pub origin: String,
}

impl SstFile {
    pub fn overlaps(&self, smallest: &str, largest: &str) -> bool {
        if self.largest.as_str() < smallest || self.smallest.as_str() > largest {
            return false;
        }
        true
    }

    pub fn contains_key(&self, key: &str) -> bool {
        if key < self.smallest.as_str() {
            return false;
        }
        if key > self.largest.as_str() {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: Seq,
    pub created_epoch_ms: u64,
    pub active: bool,
    pub note: String,
}

/// memtable flush 摘要：从若干 WAL 记录产生 L0 文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlushSpec {
    pub wal_ids: Vec<String>,
    pub level: u32,
    pub size_hint: Option<SizeHint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlushRecord {
    pub id: String,
    pub spec: FlushSpec,
    pub output_file_id: String,
}

/// 压实资源预算（派生层参数，携带 entry 单位基准）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionBudget {
    pub max_entries_per_output: u32,
    pub max_outputs: u32,
    pub size: Option<crate::domain::units::Measured>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Params {
    pub version: String,
    pub note: String,
    pub budget: CompactionBudget,
}

/// 父级依赖：所有计算结果都记录输入摘要、参数版本和父级依赖。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub input_files: Vec<String>,
    pub input_wal: Vec<String>,
    pub snapshots: Vec<String>,
    pub params_version: String,
    pub parents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DropReason {
    pub entry_id: String,
    pub key: String,
    pub seq: Seq,
    pub op: OpType,
    pub reason_code: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeRetentionNote {
    pub fragment_id: String,
    pub reason_code: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitBoundary {
    pub index: u32,
    pub after_key: String,
    pub next_key: String,
    pub entries_before: u32,
    pub constraint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSummary {
    pub file_id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    pub end_inclusive: bool,
    pub entry_count: u32,
    pub entries: Vec<Entry>,
    pub range_fragments: Vec<RangeFragment>,
    pub drops: Vec<DropReason>,
    pub split: Option<SplitBoundary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PickReason {
    pub file_id: String,
    pub level: u32,
    pub role: String,
    pub reason_code: String,
    pub detail: String,
}

/// 一次压实计算（候选结果，可能采纳也可能否定；重复安装被检测）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionResult {
    pub id: String,
    pub job_name: String,
    pub target_level: u32,
    pub inputs: Vec<String>,
    pub pick_reasons: Vec<PickReason>,
    pub outputs: Vec<OutputSummary>,
    pub range_retention: Vec<RangeRetentionNote>,
    pub provenance: Provenance,
    pub status: CompactionStatus,
    /// 安装序号：None 表示候选；已安装时记录安装事件 id（重复安装据此拒绝）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub installed_by: Option<String>,
    /// 语义无解时保留全部合法候选。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub candidates: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    Proposed,
    Interrupted,
    Installed,
    Rejected,
}
