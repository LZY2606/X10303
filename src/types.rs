//! 领域类型：内部键 / snapshot / SSTable 摘要 / 原始量 / 事件与证据。
//!
//! 设计约束（对应需求）：
//! * 所有领域量携带单位或坐标基准；导入时保留原始表示，规范化只发生在派生层。
//! * 资源预算耗尽、格式损坏、语义无解是三种互不同构的结果（[ComputeOutcome]）。
//! * 输入不完整返回带位置的诊断，不用空值/默认值伪装成功。

use serde::{Deserialize, Serialize};

/// 全局序列号，坐标基准：`global-sequence-number`（单调递增，无符号 64 位）。
pub type Seq = u64;

/// 内部键值类型。序号相同时的内部顺序为 `RangeDel > Del > Put`
/// （RocksDB 风格：type 值更大者排在前面），该约定以规则版本形式显式记录，
/// 同序号多类型仍属于语义无解，需要人工决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Put = 0,
    Del = 1,
    RangeDel = 2,
}

impl Kind {
    pub fn code(self) -> u8 {
        match self {
            Kind::Put => 0,
            Kind::Del => 1,
            Kind::RangeDel => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Kind::Put => "put",
            Kind::Del => "point_delete",
            Kind::RangeDel => "range_delete",
        }
    }
}

/// 原始量：保留导入时的原始表示；`normalized` 仅在派生层计算。
/// 无法转换时原值留在原地并附诊断，不允许为画图丢弃。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawQuantity {
    pub raw: String,
    pub unit: Option<String>,
    pub base_unit: Option<String>,
    pub normalized: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl RawQuantity {
    pub fn bytes(raw: &str) -> Self {
        let raw = raw.to_string();
        let trimmed = raw.trim();
        let (num_part, unit) = match trimmed.split_once(' ') {
            Some((n, u)) => (n.trim().to_string(), Some(u.trim().to_string())),
            None => (trimmed.clone(), None),
        };
        let (normalized, diagnostic) = match num_part.parse::<f64>() {
            Ok(n) => {
                let mul = match unit.as_deref() {
                    Some("B") | Some("byte") | Some("bytes") | None => Some(1.0),
                    Some("KiB") => Some(1024.0),
                    _ => None,
                };
                match mul {
                    Some(m) => (Some(n * m), None),
                    None => (
                        None,
                        Some(format!("unknown unit {:?}; raw value kept in place", unit)),
                    ),
                }
            }
            Err(_) => (
                None,
                Some(format!("unparseable number {:?}; raw value kept in place", num_part)),
            ),
        };
        RawQuantity {
            raw,
            unit,
            base_unit: if normalized.is_some() { Some("B".to_string()) } else { None },
            normalized,
            diagnostic,
        }
    }
}

/// 坐标基准：所有键的比较都在该基准下进行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Basis {
    pub key_order: String,
    pub key_order_note: String,
    pub sequence_basis: String,
    pub size_base_unit: String,
    pub range_semantics: String,
}

impl Default for Basis {
    fn default() -> Self {
        Basis {
            key_order: "bytewise-lexicographic".into(),
            key_order_note: "user keys compared as UTF-8 bytes, lexicographic ascending".into(),
            sequence_basis: "global-sequence-number (uint64, larger seq is newer)".into(),
            size_base_unit: "byte (B)".into(),
            range_semantics: "[start,end) for range tombstones; end-exclusive".into(),
        }
    }
}

/// 来源位置：任何诊断都必须能定位到材料与偏移/字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub material_id: String,
    pub offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#offset{}", self.material_id, self.offset)?;
        if let Some(l) = self.line {
            write!(f, ":line{}", l)?;
        }
        if let Some(fld) = &self.field {
            write!(f, ":field({})", fld)?;
        }
        Ok(())
    }
}

/// 内部条目（内部键 = 用户键 + 序列号 + 类型）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub entry_id: String,
    pub key: String,
    pub seq: Seq,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// RANGEDEL 的排他终点；端点语义 [start, end)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_end: Option<String>,
    pub origin: Origin,
}

impl Entry {
    /// 内部键顺序：用户键升序 -> 序列号降序 -> 类型降序。
    pub fn internal_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| other.seq.cmp(&self.seq))
            .then_with(|| other.kind.cmp(&self.kind))
    }
    /// 粗估条目大小（字节），用于压实预算的派生计算；单位明确为 byte。
    pub fn est_bytes(&self) -> f64 {
        (self.key.len() as f64)
            + 8.0
            + 1.0
            + self.value.as_ref().map(|v| v.len() as f64).unwrap_or(0.0)
            + self.range_end.as_ref().map(|v| v.len() as f64).unwrap_or(0.0)
    }
}

/// SSTable 键范围。point-only 文件用 `Inclusive`；携带 range tombstone 时
/// 终点可能为 `Exclusive`（内部键边界可延伸到最后一个 point key 之外）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "end", content = "bound", rename_all = "snake_case")]
pub enum EndBound {
    Inclusive(String),
    Exclusive(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRange {
    pub start: String,
    pub bound: EndBound,
}

impl KeyRange {
    pub fn point_inclusive(end: &str) -> Self {
        KeyRange { start: end.to_string(), bound: EndBound::Inclusive(end.to_string()) }
    }
    pub fn inclusive(start: &str, end: &str) -> Self {
        KeyRange { start: start.into(), bound: EndBound::Inclusive(end.into()) }
    }
    pub fn exclusive(start: &str, end: &str) -> Self {
        KeyRange { start: start.into(), bound: EndBound::Exclusive(end.into()) }
    }
    /// 范围是否在键 `k` 之前结束（即 k 不在范围内且 k 更大）。
    pub fn ends_before(&self, k: &str) -> bool {
        match &self.bound {
            EndBound::Inclusive(e) => e < k,
            EndBound::Exclusive(e) => e <= k,
        }
    }
    pub fn contains(&self, k: &str) -> bool {
        self.start <= k && !self.ends_before(k)
    }
    pub fn overlaps(&self, other: &KeyRange) -> bool {
        !self.ends_before(&other.start) && !other.ends_before(&self.start)
    }
    /// 由一组条目推导文件键范围（range tomb 的起止都参与边界）。
    pub fn from_entries(entries: &[Entry]) -> Option<KeyRange> {
        let mut start: Option<String> = None;
        let mut end: Option<EndBound> = None;
        for e in entries {
            start = Some(match start {
                Some(s) => s.min(e.key.clone()),
                None => e.key.clone(),
            });
            let cand = if let Some(re) = &e.range_end {
                EndBound::Exclusive(re.clone())
            } else {
                EndBound::Inclusive(e.key.clone())
            };
            end = Some(match end {
                None => cand,
                Some(prev) => end_bound_max(prev, cand),
            });
        }
        start.map(|s| KeyRange { start: s, bound: end.unwrap() })
    }
}

/// 比较两个范围终点哪个“延伸得更远”。
pub fn end_bound_max(a: EndBound, b: EndBound) -> EndBound {
    let ka = match &a {
        EndBound::Inclusive(k) | EndBound::Exclusive(k) => k,
    };
    let kb = match &b {
        EndBound::Inclusive(k) | EndBound::Exclusive(k) => k,
    };
    match ka.cmp(kb) {
        std::cmp::Ordering::Greater => a,
        std::cmp::Ordering::Less => b,
        std::cmp::Ordering::Equal => match (&a, &b) {
            (EndBound::Inclusive(_), EndBound::Exclusive(_)) => a,
            _ => b,
        },
    }
}

/// 压实规则版本（参数修订产生新版本，历史判定永久绑定当时版本）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    pub version: u32,
    pub grandparent_overlap_budget: RawQuantity,
    pub hard_budget: bool,
    pub same_seq_policy: String,
    pub note: String,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            version: 1,
            grandparent_overlap_budget: RawQuantity::bytes("100 B"),
            hard_budget: false,
            same_seq_policy: "semantic_conflict_requires_human_decision".into(),
            note: "initial review rules".into(),
        }
    }
}

/// 导入/解析期结果：格式损坏 vs 输入不完整，二者不同构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum LoadFailure {
    Corrupt {
        origin: Origin,
        reason: String,
        raw_snippet: String,
    },
    Incomplete {
        origin: Origin,
        missing_field: String,
        raw_snippet: String,
    },
}

/// 压实计算结果三分支：成功（带产物）/ 预算耗尽 / 语义无解。
/// 格式损坏在导入期由 [LoadFailure] 表示，不进入压实。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ComputeOutcome<T> {
    Ok(T),
    BudgetExhausted {
        subject: String,
        budget: RawQuantity,
        observed: RawQuantity,
        reason: String,
        blockers: Vec<String>,
    },
    SemanticConflict {
        subject: String,
        reason: String,
        legal_candidates: Vec<String>,
    },
}
