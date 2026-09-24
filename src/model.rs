//! 领域模型：内部键、point/range tombstone、容器（WAL/memtable/SST）、快照、参数、诊断。
//! 所有“原始表示”保留在 `*_raw` 字段；规范化只发生在派生层。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// 点记录的类型。Set=写入值，Delete=点墓碑。
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Set,
    Delete,
}

/// 一条内部键值记录（内部键 = user_key + sequence + kind）。
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct PointRec {
    pub key: String,
    pub seq: u64,
    pub kind: PointKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// 领域量单位 / 坐标基准，例如 "bytes@lex"、"ms@wall"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// 来源定位（材料 id + 行号），用于从最终状态反查材料。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

/// 范围删除记录（range tombstone）。端点是否包含端点按导入原样保留。
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct RangeRec {
    pub start_key: String,
    pub start_inclusive: bool,
    pub end_key: String,
    pub end_inclusive: bool,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    Wal,
    Memtable,
    Sst,
}

/// 一个层级文件 / 内存结构。level: -2=WAL, -1=memtable, 0..=SSTable 层。
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Container {
    pub id: String,
    pub kind: ContainerKind,
    pub level: i32,
    pub name: String,
    #[serde(default)]
    pub points: Vec<PointRec>,
    #[serde(default)]
    pub ranges: Vec<RangeRec>,
    /// 导入时声明的 key range（原始表示保留）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_end_exclusive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_raw: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
    /// 该文件坐标 / 时间基准说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct SnapshotSpec {
    pub id: String,
    pub name: String,
    pub seq: u64,
    /// 活跃（持有中）的旧 snapshot 会阻止其 seq 及以上版本被丢弃。
    pub alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_basis: Option<String>,
}

/// 压实参数（带版本号；每次修订产生新版本，计算结果记录所用版本）。
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct CompactionParams {
    pub version: String,
    /// 输出 SST 目标切分阈值（bytes，单位明确）。
    pub max_output_bytes: u64,
    /// 是否允许丢弃“低于所有活跃快照”的墓碑。
    pub drop_tombstones_below_oldest_snapshot: bool,
    /// 触发 L0 压实的重叠文件数阈值。
    pub l0_compaction_threshold: usize,
    /// 单次压实资源预算（最多输出文件数）。
    pub max_output_files: usize,
}

impl Default for CompactionParams {
    fn default() -> Self {
        CompactionParams {
            version: "params_v0_baseline".to_string(),
            max_output_bytes: 120,
            drop_tombstones_below_oldest_snapshot: true,
            l0_compaction_threshold: 2,
            max_output_files: 8,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// 带位置的诊断；输入不完整时绝不以空值/默认值伪装成功。
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub location: String,
    pub message: String,
    #[serde(default)]
    pub related_ids: Vec<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, location: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            code: code.into(),
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
            related_ids: vec![],
        }
    }
    pub fn warn(code: impl Into<String>, location: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            code: code.into(),
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
            related_ids: vec![],
        }
    }
}

/// user key 排序：按原始字节字典序（坐标基准 = lexicographic bytes）。
pub fn key_cmp(a: &str, b: &str) -> Ordering {
    a.as_bytes().cmp(b.as_bytes())
}

/// 内部键顺序：user key 升序；同 key 的 seq 降序；
/// 同 (key,seq) 多类型时 Set(1) 排在 Delete(0) 之前（确定性决胜），
/// 但这种情况会被标记为歧义（见 engine/compaction）。
pub fn internal_order(a: &PointRec, b: &PointRec) -> Ordering {
    match key_cmp(&a.key, &b.key) {
        Ordering::Equal => match b.seq.cmp(&a.seq) {
            Ordering::Equal => type_rank(b.kind).cmp(&type_rank(a.kind)),
            other => other,
        },
        other => other,
    }
}

fn type_rank(k: PointKind) -> u8 {
    match k {
        PointKind::Set => 1,
        PointKind::Delete => 0,
    }
}

/// 范围删除是否覆盖某个 key（端点包含关系按记录原样）。
pub fn range_covers(r: &RangeRec, key: &str) -> bool {
    let start_ok = match key_cmp(&r.start_key, key) {
        Ordering::Less => true,
        Ordering::Equal => r.start_inclusive,
        Ordering::Greater => false,
    };
    let end_ok = match key_cmp(key, &r.end_key) {
        Ordering::Less => true,
        Ordering::Equal => r.end_inclusive,
        Ordering::Greater => false,
    };
    start_ok && end_ok
}

/// 容器实际覆盖的 key 下界（含墓碑端点）。
pub fn effective_start(c: &Container) -> Option<String> {
    let mut cand: Vec<&str> = c.points.iter().map(|p| p.key.as_str()).collect();
    cand.extend(c.ranges.iter().map(|r| r.start_key.as_str()));
    cand.min_by(|a, b| key_cmp(a, b)).map(|s| s.to_string())
}

/// 容器实际覆盖的 key 上界（含墓碑端点，即使端点排他也作为边界坐标）。
pub fn effective_end(c: &Container) -> Option<String> {
    let mut cand: Vec<&str> = c.points.iter().map(|p| p.key.as_str()).collect();
    cand.extend(c.ranges.iter().map(|r| r.end_key.as_str()));
    cand.max_by(|a, b| key_cmp(a, b)).map(|s| s.to_string())
}

/// 两个 key 区间（均为 end-exclusive）是否重叠（含邻接不算重叠）。
pub fn ranges_overlap(a_start: &str, a_end_exclusive: &str, b_start: &str, b_end_exclusive: &str) -> bool {
    key_cmp(a_start, b_end_exclusive) == Ordering::Less && key_cmp(b_start, a_end_exclusive) == Ordering::Less
}

/// 把 [start,end_inclusive] 端点转成 end-exclusive 表示。字节序 key 没有通用“后继”，
/// 因此这里只用于展示/比较，转换不了时返回 None 并保留原表示。
pub fn inclusive_to_exclusive_end(_end_inclusive: &str) -> Option<String> {
    None
}
