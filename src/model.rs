//! 领域模型：内部键、条目、SSTable 摘要、快照、诊断与事件。
//! 所有领域量保留原始表示（raw + unit/basis），派生层才做规范化。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub type SeqNo = u64;

/// 操作类型。同 (key, seq) 下按 rank 决定可见性：Put > Del > RangeDel。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    Put,
    Del,
    RangeDel,
}

impl OpKind {
    pub fn rank(self) -> u8 {
        match self {
            OpKind::Put => 2,
            OpKind::Del => 1,
            OpKind::RangeDel => 0,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Put => "put",
            OpKind::Del => "del",
            OpKind::RangeDel => "rangedel",
        }
    }
}

/// 一条内部条目（point put / point del / range del）。
/// `value` 为原始表示，`unit` 为其单位，`basis` 为 key 的坐标基准。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>, // range del 的排他端点
    pub seq: SeqNo,
    pub kind: OpKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default)]
    pub basis: String,
    #[serde(default)]
    pub source: String,
}

impl Entry {
    pub fn size_bytes(&self) -> u64 {
        (self.key.len()
            + self.end.as_deref().unwrap_or("").len()
            + self.value.as_deref().unwrap_or("").len()
            + 24) as u64
    }
    /// range del 覆盖判断：start <= key < end 且墓碑 seq 不早于目标。
    pub fn covers(&self, key: &str, seq: SeqNo) -> bool {
        self.kind == OpKind::RangeDel
            && self.key.as_str() <= key
            && key < self.end.as_deref().unwrap_or("")
            && self.seq >= seq
    }
}

/// 内部键顺序：user key 升序、seq 降序、类型 rank 降序。
pub fn cmp_internal(a: &Entry, b: &Entry) -> Ordering {
    a.key
        .cmp(&b.key)
        .then(b.seq.cmp(&a.seq))
        .then(b.kind.rank().cmp(&a.kind.rank()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: SeqNo,
}

/// SSTable 摘要：key range、level、seq 范围、来源均被保存。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SstFile {
    pub id: String,
    pub level: u32,
    pub entries: Vec<Entry>,
    pub source: String,
    pub created_by: String, // 产生该文件的事件 id
}

impl SstFile {
    pub fn smallest_key(&self) -> &str {
        self.entries.first().map(|e| e.key.as_str()).unwrap_or("")
    }
    pub fn largest_key(&self) -> String {
        self.entries
            .iter()
            .map(|e| e.end.clone().unwrap_or_else(|| e.key.clone()))
            .max()
            .unwrap_or_default()
    }
    pub fn size_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size_bytes()).sum()
    }
    pub fn seq_range(&self) -> (SeqNo, SeqNo) {
        let lo = self.entries.iter().map(|e| e.seq).min().unwrap_or(0);
        let hi = self.entries.iter().map(|e| e.seq).max().unwrap_or(0);
        (lo, hi)
    }
    pub fn tombstones(&self) -> Vec<&Entry> {
        self.entries.iter().filter(|e| e.kind != OpKind::Put).collect()
    }
}

/// 带位置的诊断：输入不完整/无法转换时记录，绝不用空值伪装成功。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub position: String,
    pub kind: String,
    pub detail: String,
    pub raw: String,
}

/// 机器可读取的事件：判断依据、输入摘要、参数版本、父级依赖。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub n: u64,
    pub kind: String,
    pub summary: String,
    pub payload: serde_json::Value,
    pub parents: Vec<String>,
    pub params_version: String,
}

/// 可修改决定（与不可变材料分层）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub subject: String,
    pub action: String, // accept | reject | adjust
    pub note: String,
    pub author: String,
    pub retracted: bool,
    pub payload: serde_json::Value,
}
