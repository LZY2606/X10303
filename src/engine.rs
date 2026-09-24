//! LSM 语义引擎：全部为纯函数，可被测试直接调用，不依赖 SQLite/Axum。
//!
//! 覆盖：
//! - 读取按内部键顺序与 snapshot 判可见性（点删除、范围删除、同序号多类型歧义）；
//! - 压实选择（重叠 L0 传递展开 + 下层重叠展开）与“选择理由”机器可读事件；
//! - point/range tombstone 的保留、丢弃与下沉判定（不能越过未参与压实的旧数据提前丢弃）；
//! - 输出切分与不重叠层约束；预算耗尽 / 格式损坏 / 语义无解三种不同结果。

use crate::domain::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engine {
    pub params: CompactionParams,
}

/// 一条机器可读判定：稳定身份 + 输入摘要 + 父级依赖，UI 每个显示值都挂它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Judgment {
    pub id: String,
    pub kind: String,
    pub severity: String,
    pub subject: String,
    pub claim: String,
    pub input_digest: String,
    pub param_version: String,
    pub parents: Vec<String>,
    pub evidence: Vec<String>,
}

impl Judgment {
    #[allow(clippy::too_many_arguments)]
    fn new(id: &str, kind: &str, severity: &str, subject: &str, claim: &str,
           digest: &str, pv: &str, parents: Vec<String>, evidence: Vec<String>) -> Self {
        Judgment {
            id: id.to_string(), kind: kind.into(), severity: severity.into(), subject: subject.into(),
            claim: claim.into(), input_digest: digest.to_string(), param_version: pv.into(),
            parents, evidence,
        }
    }
}

/// 版本链上一个版本的可见性结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionStep {
    pub record_key: String,
    pub origin: OriginRef,
    pub seq: Seq,
    pub kind: EntryKind,
    pub value: Option<String>,
    pub visible: bool,
    pub reason_code: String,
    pub reason: String,
    pub covering_range: Option<String>,
}

/// 单个 key 的读取结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    pub key: DomKey,
    pub snapshot_id: String,
    pub snapshot_seq: Seq,
    pub outcome: String,
    pub visible_kind: Option<EntryKind>,
    pub visible_value: Option<String>,
    pub ambiguity: bool,
    pub candidates: Vec<String>,
    pub chain: Vec<VersionStep>,
    pub judgments: Vec<Judgment>,
}

/// 压实选择结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub anchor_ssts: Vec<String>,
    pub selected: Vec<String>,
    pub target_level: u32,
    pub reasons: Vec<Judgment>,
    /// 被排除（未参与压实）的旧文件——tombstone 下沉判定的关键证据。
    pub excluded_below: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// 单个输出 SSTable（含边界、承载的记录与范围删除）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    pub id: String,
    pub file_no: u64,
    pub level: u32,
    pub smallest: DomKey,
    pub largest: DomKey,
    pub size_bytes: u64,
    pub point_seqs: Vec<Seq>,
    pub ranges: Vec<RangeTombstone>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionResult {
    pub plan_id: String,
    pub scenario: String,
    pub outcome: Outcome,
    pub selection: Selection,
    pub outputs: Vec<OutputFile>,
    /// 输出边界（按顺序），UI“输出边界”视图直接读它。
    pub boundaries: Vec<String>,
    pub decisions: Vec<Judgment>,
    pub drop_log: Vec<DropEntry>,
    pub diagnostics: Vec<Diagnostic>,
    pub consumed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropEntry {
    pub record: String,
    pub seq: Seq,
    pub action: String,
    pub why: String,
}

// ---------- 内部键顺序：把材料（WAL + 各层 SSTable）聚成按 key 的版本链 ----------

#[derive(Debug, Clone)]
struct ChainRecord {
    seq: Seq,
    kind: EntryKind,
    value: Option<String>,
    origin: OriginRef,
    source_id: String,
}

fn gather_chains(m: &Materials) -> (BTreeMap<Vec<u8>, Vec<ChainRecord>>, Vec<DomKey>, Vec<Diagnostic>) {
    let mut chains: BTreeMap<Vec<u8>, Vec<ChainRecord>> = BTreeMap::new();
    let mut keys: BTreeMap<Vec<u8>, DomKey> = BTreeMap::new();
    let mut diags = Vec::new();

    let mut push = |k: &DomKey, r: ChainRecord, diags: &mut Vec<Diagnostic>| {
        match k.order_bytes() {
            Some(b) => { keys.entry(b.clone()).or_insert_with(|| k.clone()); chains.entry(b).or_default().push(r); }
            None => diags.push(Diagnostic::warn(
                "E-KEY-COORD",
                format!("key raw={:?} coordinate={}", k.raw, k.coordinate),
                "坐标基准不可转换：该键保留在原位置但不参与排序/画图",
                Some(k.raw.clone()))),
        }
    };

    for w in &m.wal {
        push(&w.key, ChainRecord { seq: w.seq, kind: w.kind, value: w.value.clone(),
            origin: OriginRef { source_kind: "wal".into(), source_id: w.id.clone(), position: w.wal_offset },
            source_id: w.id.clone() }, &mut diags);
    }
    for s in &m.ssts {
        for p in &s.points {
            let k = key_for(s, p.origin.position);
            push(&k, ChainRecord { seq: p.seq, kind: p.kind, value: p.value.clone(),
                origin: p.origin.clone(), source_id: s.id.clone() }, &mut diags);
        }
    }

    for v in chains.values_mut() {
        v.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| type_rank(a.kind).cmp(&type_rank(b.kind))).then_with(|| a.value.cmp(&b.value)));
    }
    let ordered_keys: Vec<DomKey> = keys.into_values().collect();
    (chains, ordered_keys, diags)
}

/// 从 SSTable 摘要里按 position 取回 user key（点记录的 key 由 key range 摘要索引）。
fn key_for(_s: &SstSummary, _pos: usize) -> DomKey {
    // 见 domain：点记录按 SSTable 中 point_keys 的位置保存（见 case 包约定）。
    unreachable!("key_for 由 materials_with_keys 替代")
}
