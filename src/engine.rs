//! 压实引擎：读取按内部键顺序与 snapshot 判可见性；
//! 范围删除不能越过未参与压实的旧数据而提前丢弃；
//! 输出切分仍需维持不重叠层约束。
//! 资源预算耗尽、格式损坏、语义无解是三种不同结果。

use crate::domain::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 三种必须区分的失败结果。
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineError {
    /// 资源预算耗尽（如输出大小预算、步数预算）
    #[error("budget exhausted: {0}")]
    BudgetExhausted(String),
    /// 格式损坏（输入片段无法解析）
    #[error("corrupt format: {0}")]
    CorruptFormat(String),
    /// 语义无解（约束互相冲突，无合法输出）
    #[error("semantic conflict: {0}")]
    SemanticConflict(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionBudget {
    /// 最大输出字节数
    pub max_output_bytes: u64,
    /// 最大处理记录数（用于模拟压实中断）
    pub max_records: Option<usize>,
}

impl Default for CompactionBudget {
    fn default() -> Self {
        CompactionBudget { max_output_bytes: u64::MAX, max_records: None }
    }
}

/// 压实输入摘要（保存进事件与结果）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionInput {
    pub input_files: Vec<SstFile>,
    pub snapshots: Vec<Snapshot>,
    pub params: CompactionParams,
    pub budget: CompactionBudget,
    /// 压实是否下沉到最底层（决定墓碑能否丢弃）
    pub bottommost_level: bool,
    /// 未参与压实的旧数据 key range（范围删除不得越过它们提前丢弃）
    pub uninvolved_ranges: Vec<(String, String)>,
}

/// 单条输出记录及其保留/丢弃理由。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEntry {
    pub record: Record,
    pub decision: EntryDecision,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryDecision {
    Kept,
    DroppedObsolete,
    DroppedShadowed,
    DroppedTombstoneBottommost,
    DroppedRangeCovered,
}

/// 压实输出摘要（保存进事件与结果）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionOutput {
    pub files: Vec<SstFile>,
    pub entries: Vec<OutputEntry>,
    pub interrupted: bool,
    pub input_digest: String,
    pub params_version: u32,
}

/// 读取视图：按内部键顺序遍历，按 snapshot 判可见性。
/// 返回每个 user_key 在 snapshot 下可见的版本（若有）。
pub fn visible_at(files: &[SstFile], snap_seq: u64) -> Vec<(String, Option<String>)> {
    let mut recs: Vec<Record> = files.iter().flat_map(|f| f.records.clone()).collect();
    recs.sort_by(|a, b| a.ikey.cmp(&b.ikey));
    let range_dels: Vec<&Record> = recs.iter().filter(|r| r.is_range_del() && r.ikey.seq <= snap_seq).collect();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut current_key: Option<String> = None;
    let mut resolved = false;
    for r in &recs {
        if r.is_range_del() || r.ikey.seq > snap_seq {
            continue;
        }
        if current_key.as_deref() != Some(r.ikey.user_key.as_str()) {
            current_key = Some(r.ikey.user_key.clone());
            resolved = false;
        }
        if resolved {
            continue;
        }
        // 范围删除端点：seq <= snap 的 RDEL 覆盖本 key 时，本版本不可见
        let covered = range_dels.iter().any(|rd| rd.covers(&r.ikey.user_key) && rd.ikey.seq >= r.ikey.seq);
        resolved = true;
        if covered {
            out.push((r.ikey.user_key.clone(), None));
        } else {
            match r.ikey.kind {
                KIND_POINT => out.push((r.ikey.user_key.clone(), r.value.clone())),
                KIND_DELETE => out.push((r.ikey.user_key.clone(), None)),
                _ => {}
            }
        }
    }
    out
}

/// 执行压实。等价输入得到稳定顺序输出（输入先按 (level, id) 排序，记录按内部键排序）。
pub fn compact(input: &CompactionInput) -> Result<CompactionOutput, EngineError> {
    validate(input)?;
    let mut files = input.input_files.clone();
    files.sort_by(|a, b| (a.level, a.id.clone()).cmp(&(b.level, b.id.clone())));
    let mut recs: Vec<Record> = files.iter().flat_map(|f| f.records.clone()).collect();
    recs.sort_by(|a, b| a.ikey.cmp(&b.ikey));

    let snap_seqs: Vec<u64> = input.snapshots.iter().filter(|s| s.active).map(|s| s.seq).collect();
    let oldest_snap = snap_seqs.iter().min().copied();

    let mut entries: Vec<OutputEntry> = Vec::new();
    let mut interrupted = false;
    let mut processed = 0usize;

    // 同序号多类型：内部键序保证同 (user_key, seq) 时 PUT 在 DEL 前，均保留各自语义。
    // 可见性去重：每个 user_key 只保留每个快照区间内的最新版本。
    let mut last_key: Option<String> = None;
    let mut kept_seq_for_key: Vec<u64> = Vec::new();
    let range_dels: Vec<Record> = recs.iter().filter(|r| r.is_range_del()).cloned().collect();

    for r in &recs {
        if let Some(max) = input.budget.max_records {
            if processed >= max {
                interrupted = true;
                break;
            }
        }
        processed += 1;

        if r.is_range_del() {
            // 范围删除不能越过未参与压实的旧数据而提前丢弃
            let crosses_uninvolved = input
                .uninvolved_ranges
                .iter()
                .any(|(lo, hi)| r.ikey.user_key.as_str() <= hi.as_str() && r.range_end.as_deref().unwrap_or("") >= lo.as_str());
            if input.bottommost_level && input.params.allow_range_del_elision && !crosses_uninvolved {
                entries.push(OutputEntry {
                    record: r.clone(),
                    decision: EntryDecision::DroppedRangeCovered,
                    reason: "bottommost 且不与未参与压实的旧数据重叠，范围删除可丢弃".into(),
                });
            } else {
                let reason = if crosses_uninvolved {
                    "与未参与压实的旧数据重叠，范围删除必须下沉保留".to_string()
                } else {
                    "非最底层压实，范围删除下沉保留".to_string()
                };
                entries.push(OutputEntry { record: r.clone(), decision: EntryDecision::Kept, reason });
            }
            continue;
        }

        let key = &r.ikey.user_key;
        if last_key.as_deref() != Some(key.as_str()) {
            last_key = Some(key.clone());
            kept_seq_for_key.clear();
        }

        // 活跃旧 snapshot：若某快照需要看到本版本，则必须保留
        let needed_by_snapshot = oldest_snap.map_or(false, |s| {
            // 本版本是快照 s 可见的最新版本：seq <= s 且没有已保留的 (s, seq] 区间版本
            r.ikey.seq <= s && !kept_seq_for_key.iter().any(|k| *k > r.ikey.seq && *k <= s)
        });

        let shadowed_by_range_del = range_dels
            .iter()
            .any(|rd| rd.covers(key) && rd.ikey.seq >= r.ikey.seq);

        if !kept_seq_for_key.is_empty() && !needed_by_snapshot {
            entries.push(OutputEntry {
                record: r.clone(),
                decision: EntryDecision::DroppedObsolete,
                reason: "同 key 已有更新版本保留，且无活跃快照需要本版本".into(),
            });
            continue;
        }

        if shadowed_by_range_del && !needed_by_snapshot {
            entries.push(OutputEntry {
                record: r.clone(),
                decision: EntryDecision::DroppedShadowed,
                reason: "被更新序号范围删除覆盖，且无活跃快照需要本版本".into(),
            });
            continue;
        }

        if r.ikey.kind == KIND_DELETE && input.bottommost_level && !needed_by_snapshot {
            entries.push(OutputEntry {
                record: r.clone(),
                decision: EntryDecision::DroppedTombstoneBottommost,
                reason: "point tombstone 下沉到最底层且无快照引用，可丢弃".into(),
            });
            continue;
        }

        let reason = if needed_by_snapshot && !kept_seq_for_key.is_empty() {
            format!("活跃旧 snapshot (seq<={}) 需要本版本，保留", oldest_snap.unwrap_or(0))
        } else if r.ikey.kind == KIND_DELETE {
            "point tombstone 未达最底层，保留以遮蔽下层旧值".to_string()
        } else {
            "可见最新版本，保留".to_string()
        };
        kept_seq_for_key.push(r.ikey.seq);
        entries.push(OutputEntry { record: r.clone(), decision: EntryDecision::Kept, reason });
    }

    // 输出切分：按 target_file_size 切分，切分点只在 user_key 边界，
    // 维持不重叠层约束（同层输出文件 key range 互不重叠）。
    let kept: Vec<Record> = entries
        .iter()
        .filter(|e| e.decision == EntryDecision::Kept)
        .map(|e| e.record.clone())
        .collect();
    let out_level = files.iter().map(|f| f.level).max().unwrap_or(0) + 1;
    let mut out_files: Vec<SstFile> = Vec::new();
    let mut cur: Vec<Record> = Vec::new();
    let mut cur_bytes: u64 = 0;
    let mut split_index = 0usize;
    for r in kept {
        let est = estimate_bytes(&r);
        let key_boundary = cur.last().map_or(true, |p| p.ikey.user_key != r.ikey.user_key);
        if !cur.is_empty() && cur_bytes + est > input.params.target_file_size && key_boundary {
            out_files.push(make_output_file(&cur, out_level, split_index));
            split_index += 1;
            cur.clear();
            cur_bytes = 0;
        }
        cur_bytes += est;
        cur.push(r);
    }
    if !cur.is_empty() {
        out_files.push(make_output_file(&cur, out_level, split_index));
    }

    let total_bytes: u64 = out_files.iter().map(|f| f.size.value.unwrap_or(0.0) as u64).sum();
    if total_bytes > input.budget.max_output_bytes {
        return Err(EngineError::BudgetExhausted(format!(
            "输出 {}B 超出预算 {}B",
            total_bytes, input.budget.max_output_bytes
        )));
    }

    // 校验输出层不重叠约束
    for w in out_files.windows(2) {
        if w[0].largest >= w[1].smallest {
            return Err(EngineError::SemanticConflict(format!(
                "输出切分破坏不重叠层约束: {} 与 {} 重叠",
                w[0].id, w[1].id
            )));
        }
    }

    Ok(CompactionOutput {
        files: out_files,
        entries,
        interrupted,
        input_digest: digest_input(input),
        params_version: input.params.version,
    })
}

fn validate(input: &CompactionInput) -> Result<(), EngineError> {
    for f in &input.input_files {
        if f.smallest > f.largest {
            return Err(EngineError::CorruptFormat(format!(
                "文件 {} key range 倒置: {} > {}",
                f.id, f.smallest, f.largest
            )));
        }
        for r in &f.records {
            if r.is_range_del() && r.range_end.is_none() {
                return Err(EngineError::CorruptFormat(format!(
                    "文件 {} 中范围删除 {} 缺少终点",
                    f.id, r.ikey
                )));
            }
            if r.ikey.kind == KIND_POINT && r.value.is_none() {
                return Err(EngineError::CorruptFormat(format!(
                    "文件 {} 中 point 记录 {} 缺少值",
                    f.id, r.ikey
                )));
            }
        }
    }
    // 语义无解：输入文件互相矛盾（同层非 L0 文件重叠）
    let mut by_level: std::collections::BTreeMap<u32, Vec<&SstFile>> = Default::default();
    for f in &input.input_files {
        by_level.entry(f.level).or_default().push(f);
    }
    for (level, fs) in &by_level {
        if *level == 0 {
            continue; // L0 允许重叠
        }
        for i in 0..fs.len() {
            for j in (i + 1)..fs.len() {
                if fs[i].overlaps(fs[j]) {
                    return Err(EngineError::SemanticConflict(format!(
                        "L{} 非 L0 层文件 {} 与 {} key range 重叠",
                        level, fs[i].id, fs[j].id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn estimate_bytes(r: &Record) -> u64 {
    (r.ikey.user_key.len()
        + 8
        + 1
        + r.value.as_ref().map_or(0, |v| v.len())
        + r.range_end.as_ref().map_or(0, |v| v.len())) as u64
}

fn make_output_file(records: &[Record], level: u32, index: usize) -> SstFile {
    let smallest = records.first().map(|r| r.ikey.user_key.clone()).unwrap_or_default();
    let largest = records.last().map(|r| r.ikey.user_key.clone()).unwrap_or_default();
    let bytes: u64 = records.iter().map(estimate_bytes).sum();
    SstFile {
        id: format!("out-L{}-{:04}", level, index),
        level,
        smallest,
        largest,
        records: records.to_vec(),
        size: Quantity::bytes(bytes),
        source: "compaction-output".into(),
    }
}

pub fn digest_input(input: &CompactionInput) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(&(
        &input.input_files,
        &input.snapshots,
        &input.params,
        input.bottommost_level,
        &input.uninvolved_ranges,
    ))
    .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(json.as_bytes());
    format!("sha256:{:x}", h.finalize())
}

/// 压实选择理由：解释为何选择这些输入文件。
pub fn pick_compaction(levels: &[LevelView], params: &CompactionParams) -> Option<(Vec<String>, String)> {
    let l0 = levels.iter().find(|l| l.level == 0)?;
    if l0.files.len() >= params.l0_trigger {
        let ids: Vec<String> = l0.files.iter().map(|f| f.id.clone()).collect();
        return Some((
            ids.clone(),
            format!(
                "L0 文件数 {} 达到触发阈值 {}，且 L0 允许重叠，需压实到 L1 恢复不重叠层约束",
                l0.files.len(),
                params.l0_trigger
            ),
        ));
    }
    None
}
