use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

pub fn digest_text(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\x1f");
    }
    let out = hasher.finalize();
    out[..10].iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn digest_json<T: Serialize>(value: &T) -> String {
    digest_text(&[&serde_json::to_string(value).unwrap_or_default()])
}

/// RocksDB 风格：同一 (user_key, seq) 下 Value 排在 Deletion 之前（type 降序）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    Deletion,
    Value,
}

impl ValueType {
    fn rank(self) -> u8 {
        match self {
            ValueType::Deletion => 0,
            ValueType::Value => 1,
        }
    }
}

/// 内部键排序：user_key 升序、seq 降序、type 降序。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalKey {
    pub user_key: String,
    pub seq: u64,
    pub vtype: ValueType,
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.user_key
            .cmp(&other.user_key)
            .then(other.seq.cmp(&self.seq))
            .then(other.vtype.rank().cmp(&self.vtype.rank()))
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointEntry {
    pub key: InternalKey,
    pub value: Option<String>,
}

impl Ord for PointEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

impl PartialOrd for PointEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 范围删除：[start, end) 半开区间，坐标基准为字节序字典序。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeTombstone {
    pub start: String,
    pub end: String,
    pub seq: u64,
}

impl RangeTombstone {
    pub fn covers(&self, user_key: &str) -> bool {
        self.start.as_str() <= user_key && user_key < self.end.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sstable {
    pub id: String,
    pub level: u32,
    pub points: Vec<PointEntry>,
    pub range_dels: Vec<RangeTombstone>,
}

impl Sstable {
    pub fn new(level: u32, mut points: Vec<PointEntry>, mut range_dels: Vec<RangeTombstone>) -> Sstable {
        points.sort();
        points.dedup();
        range_dels.sort_by(|a, b| {
            (&a.start, &a.end, std::cmp::Reverse(a.seq)).cmp(&(&b.start, &b.end, std::cmp::Reverse(b.seq)))
        });
        range_dels.dedup();
        let id = format!("sst_{}", digest_json(&("sstable", level, &points, &range_dels)));
        Sstable { id, level, points, range_dels }
    }

    /// 文件用户键范围（含 range_del 边界），闭区间表示。
    pub fn user_range(&self) -> Option<(String, String)> {
        let mut lo: Option<&str> = None;
        let mut hi: Option<&str> = None;
        let mut feed = |k: &str| {
            if lo.map_or(true, |l| k < l) {
                lo = Some(k);
            }
            if hi.map_or(true, |h| k > h) {
                hi = Some(k);
            }
        };
        for p in &self.points {
            feed(&p.key.user_key);
        }
        for t in &self.range_dels {
            feed(&t.start);
            feed(&t.end);
        }
        Some((lo?.to_string(), hi?.to_string()))
    }

    pub fn contains_user_key(&self, key: &str) -> bool {
        self.user_range()
            .map_or(false, |(lo, hi)| lo.as_str() <= key && key <= hi.as_str())
    }

    /// 与半开区间 [start, end) 是否相交。
    pub fn overlaps_span(&self, start: &str, end: &str) -> bool {
        self.user_range()
            .map_or(false, |(lo, hi)| lo.as_str() < end && start <= hi.as_str())
    }

    pub fn overlaps_file(&self, other: &Sstable) -> bool {
        match (self.user_range(), other.user_range()) {
            (Some((alo, ahi)), Some((blo, bhi))) => alo <= bhi && blo <= ahi,
            _ => false,
        }
    }

    pub fn size_bytes(&self) -> usize {
        32 + self
            .points
            .iter()
            .map(|p| 16 + p.key.user_key.len() + p.value.as_deref().unwrap_or("").len())
            .sum::<usize>()
            + self
                .range_dels
                .iter()
                .map(|t| 24 + t.start.len() + t.end.len())
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
}
