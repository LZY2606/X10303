//! 领域模型:LSM 内部键、点值/点墓碑、范围墓碑、SSTable 摘要、快照、压实参数。
//! 所有领域量都带坐标基准说明:key 为字节序字典序坐标,seq 为逻辑时钟序号,
//! 区间一律为 [start, end) 左闭右开。

use serde::{Deserialize, Serialize};

pub type Seq = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Put,
    Del,
}

impl EntryKind {
    /// 同 (key, seq) 下的类型排序权值:Del 视为"更新"(与 RocksDB type 降序一致)。
    pub fn rank(self) -> u8 {
        match self {
            EntryKind::Del => 1,
            EntryKind::Put => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub seq: Seq,
    pub kind: EntryKind,
    /// Put 时携带的值(原始表示,不做规范化);Del 时为 None。
    pub value: Option<String>,
    /// 来源文件身份,合并时保留,便于反查。
    pub from_file: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeDel {
    /// 覆盖区间 [start, end),坐标基准:字节序字典序。
    pub start: String,
    pub end: String,
    pub seq: Seq,
    pub from_file: String,
}

impl RangeDel {
    pub fn covers(&self, key: &str, seq: Seq) -> bool {
        self.start.as_str() <= key && key < self.end.as_str() && seq < self.seq
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sst {
    pub id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    /// wal-import | memtable-flush | compaction:<event> | seed | source-revision
    pub origin: String,
    pub entries: Vec<Entry>,
    pub range_dels: Vec<RangeDel>,
}

impl Sst {
    pub fn overlaps(&self, start: &str, end_inclusive: &str) -> bool {
        self.smallest.as_str() <= end_inclusive && self.largest.as_str() >= start
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: Seq,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tiebreak {
    /// 同 (key, seq) 多类型冲突时不做裁决 -> Unresolvable
    None,
    FileIdAsc,
    FileIdDesc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    pub version: u32,
    /// 每个输出 SSTable 的最大 entry 数(资源预算,单位:条)
    pub output_entry_budget: usize,
    /// 单次压实允许的最大输出文件数(资源预算,单位:个)
    pub max_outputs_per_compaction: usize,
    pub tiebreak: Tiebreak,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            version: 1,
            output_entry_budget: 4,
            max_outputs_per_compaction: 10,
            tiebreak: Tiebreak::None,
        }
    }
}

/// 内部键排序:用户键升序(字节序),seq 降序,类型权值降序。
pub fn internal_key_cmp(a: (&str, Seq, EntryKind), b: (&str, Seq, EntryKind)) -> std::cmp::Ordering {
    a.0.cmp(b.0)
        .then(b.1.cmp(&a.1))
        .then(b.2.rank().cmp(&a.2.rank()))
}

/// 读取路径:按内部键顺序找到第一个 seq <= snap_seq 的版本。
/// files 内全部 entry 参与;返回可见值(或墓碑=已删除)。
pub fn visible_at(files: &[&Sst], key: &str, snap_seq: Seq) -> Option<Option<String>> {
    let mut best: Option<&Entry> = None;
    for f in files {
        for e in &f.entries {
            if e.key == key && e.seq <= snap_seq {
                let better = match best {
                    None => true,
                    Some(b) => {
                        internal_key_cmp((&e.key, e.seq, e.kind), (&b.key, b.seq, b.kind))
                            == std::cmp::Ordering::Less
                    }
                };
                if better {
                    best = Some(e);
                }
            }
        }
    }
    best.map(|e| match e.kind {
        EntryKind::Put => Some(e.value.clone().unwrap_or_default()),
        EntryKind::Del => None,
    })
}
