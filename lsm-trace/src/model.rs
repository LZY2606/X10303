//! 领域核心模型：内部键、版本类型、顺序、snapshot 可见性、稳定身份。
//!
//! 约定（与 RocksDB 风格的 LSM 对齐，所有约定在事件 `ruleset` 中版本化）：
//!
//! * 内部键 = 用户键 + 序列号 seq（`u64`，单调递增，单位“逻辑写入序号”）+ 类型。
//! * 同用户键按 `seq` 降序；同 seq 用固定决胜序：
//!   PUT(0) < MERGE(1) < DELETE(2) < RANGE_DELETION(3)（序列号相同属异常，保留并诊断）。
//! * snapshot 持有一个 `last_seq`（“快照上界/ExclusiveUpper”，坐标基准：逻辑写入序号）；
//!   快照可见 `seq <= snapshot.last_seq` 的版本。
//! * 点 tombstone（DELETE）只有在不存在 `last_seq >= del.seq` 的活跃快照，
//!   且压实输入在该用户键上覆盖全部更旧数据时，才允许下沉丢弃。
//! * 范围 tombstone 端点以 `[start, end)` 表示，坐标基准为用户键的字典序空间；
//!   不能越过未参与压实的旧数据提前丢弃，允许按输出边界分片保留。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 内部版本类型；判别码同时用于同 seq 的固定决胜序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Put = 0,
    Merge = 1,
    Delete = 2,
    RangeDeletion = 3,
}

impl EntryKind {
    pub fn rank(self) -> u8 {
        self as u8
    }
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Put => "put",
            EntryKind::Merge => "merge",
            EntryKind::Delete => "delete",
            EntryKind::RangeDeletion => "range_deletion",
        }
    }
    pub fn parse(s: &str) -> Option<EntryKind> {
        match s {
            "put" => Some(EntryKind::Put),
            "merge" => Some(EntryKind::Merge),
            "delete" | "del" => Some(EntryKind::Delete),
            "range_deletion" | "range_delete" | "range_del" => Some(EntryKind::RangeDeletion),
            _ => None,
        }
    }
    pub fn is_point(self) -> bool {
        matches!(self, EntryKind::Put | EntryKind::Merge | EntryKind::Delete)
    }
    pub fn is_tombstone(self) -> bool {
        matches!(self, EntryKind::Delete | EntryKind::RangeDeletion)
    }
}

/// 一条内部键版本（点记录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalEntry {
    pub key: String,
    pub seq: u64,
    pub kind: EntryKind,
    /// 点值；DELETE/RANGE 无值，保留为 None（语义上的缺失，不是默认值伪装）。
    pub value: Option<String>,
    /// 值的单位（来自原始表示，派生层才规范化）。
    pub value_unit: Option<String>,
}

/// 端点包含性：仅支持 `[start, end)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeDesc {
    pub start: String,
    pub end: String,
}

impl RangeDesc {
    /// 空区间（含 start >= end）不产生任何覆盖，导入时诊断而非静默接受。
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
    pub fn covers(&self, key: &str) -> bool {
        self.start.as_str() <= key && key < self.end.as_str()
    }
    /// 该范围是否与 `[lo, hi)`（lo<=hi）相交。
    pub fn intersects(&self, lo: &str, hi: &str) -> bool {
        self.start.as_str() < hi && lo < self.end.as_str()
    }
    /// 与 `[lo, hi)` 求交；返回的仍是半开区间。无交返回 None。
    pub fn clip(&self, lo: &str, hi: &str) -> Option<RangeDesc> {
        let start = std::cmp::max(self.start.as_str(), lo);
        let end = std::cmp::min(self.end.as_str(), hi);
        if start < end {
            Some(RangeDesc { start: start.to_string(), end: end.to_string() })
        } else {
            None
        }
    }
}

/// 内部键排序：key 升序 → seq 降序 → 类型决胜序升序。
pub fn internal_cmp(a: &InternalEntry, b: &InternalEntry) -> std::cmp::Ordering {
    a.key
        .cmp(&b.key)
        .then_with(|| b.seq.cmp(&a.seq))
        .then_with(|| a.kind.rank().cmp(&b.kind.rank()))
}

/// snapshot 上界（ExclusiveUpper）：可见 seq <= last_seq。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub last_seq: u64,
}

impl Snapshot {
    pub fn visible(&self, seq: u64) -> bool {
        seq <= self.last_seq
    }
}

/// 读取可见性判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// 被所有读视图（含最旧活跃快照）可见。
    Visible,
    /// 对最新读视图可见，但被至少一个活跃旧快照遮蔽。
    ShadowedBySnapshot,
    /// 高于最新 snapshot/读上界。
    BeyondReadUpper,
}

/// 依据“读上界 + 活跃快照集合”判定某 seq 的可见性。
///
/// * `read_upper`：当前读视图上界（通常为最新写入 seq）。
/// * `oldest_active_snapshot`：最旧的活跃快照上界（没有则为 None）。
pub fn visibility_of(seq: u64, read_upper: u64, oldest_active: Option<u64>) -> Visibility {
    if seq > read_upper {
        Visibility::BeyondReadUpper
    } else if let Some(s) = oldest_active {
        if seq <= s {
            Visibility::Visible
        } else {
            Visibility::ShadowedBySnapshot
        }
    } else {
        Visibility::Visible
    }
}

// ---- 稳定身份 ----------------------------------------------------------------

/// 规范化哈希：对 JSON 值按 key 排序后取 SHA-256 前 16 字节十六进制。
/// 等价输入在任意机器、任意进程上得到相同摘要。
pub fn stable_id(kind: &str, canonical: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b":");
    let canon = canonicalize(canonical);
    hasher.update(canon.to_string().as_bytes());
    format!("{}_{}", kind, hex::encode(&hasher.finalize()[..16]))
}

/// 递归把 JSON 对象的 key 排序，消除 `preserve_order` 带来的差异。
pub fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(&String, &Value)> = m.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (k, val) in entries {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_ordering_and_snapshot_visibility() {
        let mut v = vec![
            InternalEntry { key: "b".into(), seq: 9, kind: EntryKind::Put, value: None, value_unit: None },
            InternalEntry { key: "a".into(), seq: 2, kind: EntryKind::Put, value: None, value_unit: None },
            InternalEntry { key: "a".into(), seq: 5, kind: EntryKind::Delete, value: None, value_unit: None },
        ];
        v.sort_by(internal_cmp);
        assert_eq!(v[0].key, "a");
        assert_eq!(v[0].seq, 5); // 同 key seq 降序
        assert_eq!(v[1].key, "a");
        assert_eq!(v[2].key, "b");

        let snap = Snapshot { last_seq: 4 };
        assert!(snap.visible(4));
        assert!(!snap.visible(5));

        // 旧快照 seq=4：seq=5 的删除被旧快照遮蔽，必须保留。
        assert_eq!(visibility_of(5, 9, Some(4)), Visibility::ShadowedBySnapshot);
        assert_eq!(visibility_of(4, 9, Some(4)), Visibility::Visible);
        assert_eq!(visibility_of(10, 9, Some(4)), Visibility::BeyondReadUpper);
    }

    #[test]
    fn range_clip_semantics() {
        let r = RangeDesc { start: "b".into(), end: "e".into() };
        assert!(r.covers("b"));
        assert!(!r.covers("e"));
        assert_eq!(
            r.clip("c", "z"),
            Some(RangeDesc { start: "c".into(), end: "e".into() })
        );
        assert_eq!(r.clip("z", "zz"), None);
        assert!(!RangeDesc { start: "x".into(), end: "x".into() }.is_empty() == false);
    }

    #[test]
    fn stable_ids_are_deterministic() {
        let a = serde_json::json!({"b": 1, "a": [1, 2]});
        let b = serde_json::json!({"a": [1, 2], "b": 1});
        assert_eq!(stable_id("ev", &a), stable_id("ev", &b));
        assert_ne!(stable_id("ev", &a), stable_id("file", &b));
    }
}
