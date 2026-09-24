//! 压实选择与合并：point/range tombstone 的保留、下沉与输出切分。
//!
//! 规则（全部机器可读，且在 `Params` 中携带版本）：
//! 1. 选择：L0 种子拉入全部与其重叠的 L0（L0 允许互相重叠），再与 L1 重叠文件配对；
//!    Ln(n>=1) 种子与 Ln+1 重叠文件配对，并对输出层做不动点扩张。
//! 2. 合并按内部键顺序；同键旧版本仅在存在活跃旧 snapshot（entry.seq <= oldest.seq）时保留。
//! 3. range tombstone 在以下任一情况下保守保留并下沉：
//!    a) 活跃 snapshot 可能读到其覆盖区间（snapshot.seq >= tombstone.seq）；
//!    b) 未参与本次压实的更深层文件中，仍存在区间内、序号更旧的点数据。
//!    —— 即“范围删除不能越过未参与压实的旧数据而提前丢弃”。
//! 4. 输出按 key 边界切分；同层输出必须互不重叠，否则判 SemanticUnresolved。

use crate::domain::errors::{corrupt, LoadError, LoadResult};
use crate::domain::ids::stable_id;
use crate::domain::internals::{internal_order, oldest_active_snapshot};
use crate::domain::model::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Pick {
    pub inputs: Vec<String>,
    pub reasons: Vec<PickReason>,
    pub target_level: u32,
}

pub fn pick_inputs(files: &[SstFile], seed_id: &str) -> LoadResult<Pick> {
    let seed = files
        .iter()
        .find(|f| f.id == seed_id)
        .ok_or_else(|| corrupt("compaction.seed_file_id", format!("种子文件 {seed_id} 不存在")))?;
    let mut reasons = Vec::new();
    let mut input_ids: Vec<String> = vec![seed.id.clone()];
    reasons.push(PickReason {
        file_id: seed.id.clone(),
        level: seed.level,
        role: "seed".to_string(),
        reason_code: "seed_selected".to_string(),
        detail: format!("压实种子，key range [{}, {}]", seed.smallest, seed.largest),
    });

    let (source_level, target_level) = if seed.level == 0 { (0u32, 1u32) } else { (seed.level, seed.level + 1) };

    if source_level == 0 {
        for file in files.iter().filter(|f| f.level == 0 && f.id != seed.id) {
            if file.overlaps(&seed.smallest, &seed.largest) {
                if !input_ids.iter().any(|x| x == &file.id) {
                    input_ids.push(file.id.clone());
                }
                reasons.push(PickReason {
                    file_id: file.id.clone(),
                    level: 0,
                    role: "l0_overlap".to_string(),
                    reason_code: "l0_overlapping".to_string(),
                    detail: format!(
                        "L0 允许层内重叠：[{}, {}] 与种子 [{}, {}] 重叠，必须一起压实",
                        file.smallest, file.largest, seed.smallest, seed.largest
                    ),
                });
            }
        }
    }

    // 对目标层做不动点扩张。
    let mut frontier_lo = seed.smallest.clone();
    let mut frontier_hi = seed.largest.clone();
    loop {
        let mut changed = false;
        for file in files.iter().filter(|f| f.level == target_level) {
            let already = input_ids.iter().any(|id| id == &file.id);
            if !already && file.overlaps(&frontier_lo, &frontier_hi) {
                input_ids.push(file.id.clone());
                reasons.push(PickReason {
                    file_id: file.id.clone(),
                    level: target_level,
                    role: "target_overlap".to_string(),
                    reason_code: "output_level_overlap".to_string(),
                    detail: format!(
                        "输出层 L{target_level} 文件 [{}, {}] 与当前压实边界 [{}, {}] 重叠，纳入以维持不重叠层约束",
                        file.smallest, file.largest, frontier_lo, frontier_hi
                    ),
                });
                if file.smallest < frontier_lo {
                    frontier_lo = file.smallest.clone();
                }
                if file.largest > frontier_hi {
                    frontier_hi = file.largest.clone();
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let order: BTreeMap<&str, (u32, &str, &str)> = files
        .iter()
        .map(|f| (f.id.as_str(), (f.level, f.smallest.as_str(), f.id.as_str())))
        .collect();
    input_ids.sort_by(|a, b| order.get(a.as_str()).cmp(&order.get(b.as_str())));
    reasons.sort_by(|a, b| a.file_id.cmp(&b.file_id).then_with(|| a.reason_code.cmp(&b.reason_code)));

    Ok(Pick { inputs: input_ids, reasons, target_level })
}

#[derive(Debug, Clone)]
struct KeyGroup {
    key: String,
    entries: Vec<Entry>,
}

struct MergeArtifacts {
    groups: Vec<KeyGroup>,
    drops: Vec<DropReason>,
    fragments: Vec<(RangeFragment, Vec<RangeRetentionNote>)>,
}

fn has_older_point(deeper: &[&SstFile], key: &str, seq: Seq) -> bool {
    deeper.iter().any(|f| {
        f.contains_key(key)
            && f.entries.iter().any(|e| e.key == key && e.op == OpType::Put && e.seq < seq)
    })
}

fn merge(
    files: &[SstFile],
    pick: &Pick,
    snapshots: &[Snapshot],
) -> MergeArtifacts {
    let by_id: BTreeMap<&str, &SstFile> = files.iter().map(|f| (f.id.as_str(), f)).collect();
    let inputs: Vec<&SstFile> = pick
        .inputs
        .iter()
        .map(|id| *by_id.get(id.as_str()).expect("picked file exists"))
        .collect();

    let deeper: Vec<&SstFile> = files
        .iter()
        .filter(|f| f.level > pick.target_level && !pick.inputs.iter().any(|id| id == &f.id))
        .collect();

    let oldest = oldest_active_snapshot(snapshots);

    // 1. 点条目分组（内部键顺序），同键保留“对最老活跃 snapshot 仍可见”的旧版本。
    let mut all_entries: Vec<Entry> = inputs.iter().flat_map(|f| f.entries.iter().cloned()).collect();
    all_entries.sort_by(internal_order);
    all_entries.dedup_by(|a, b| a.entry_id == b.entry_id);

    let mut group_map: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for entry in all_entries {
        group_map.entry(entry.key.clone()).or_default().push(entry);
    }

    let mut drops = Vec::new();
    let mut groups = Vec::new();
    for (key, mut entries) in group_map {
        entries.sort_by(internal_order);
        let newest = entries[0].clone();
        let mut kept = vec![newest.clone()];
        for older in entries.iter().skip(1) {
            if let Some(snap) = oldest {
                if older.seq <= snap.seq {
                    kept.push(older.clone());
                    continue;
                }
            }
            drops.push(DropReason {
                entry_id: older.entry_id.clone(),
                key: older.key.clone(),
                seq: older.seq,
                op: older.op,
                reason_code: "shadowed_in_compaction".to_string(),
                detail: format!(
                    "seq={} 的旧版本被同键更新内部键（{} seq={}）遮蔽，且没有活跃旧 snapshot 需要它",
                    older.seq,
                    newest.op.label(),
                    newest.seq
                ),
            });
        }

        // 最新者是点 tombstone 时，判定能否安全回收。除了“无旧 snapshot / 外部无更旧数据”，
        // 还必须确认本输出中没有为旧 snapshot 保留的同键更旧条目——否则回收 tombstone
        // 会让新读者看到那些旧值（复活）。
        if newest.op == OpType::Delete {
            let protected = oldest.is_some_and(|s| newest.seq <= s.seq);
            let older_outside = has_older_point(&deeper, &newest.key, newest.seq);
            let older_kept_inside = kept.len() > 1;
            if !protected && !older_outside && !older_kept_inside {
                kept.remove(0);
                drops.push(DropReason {
                    entry_id: newest.entry_id.clone(),
                    key: newest.key.clone(),
                    seq: newest.seq,
                    op: newest.op,
                    reason_code: "point_tombstone_dropped_safe".to_string(),
                    detail: "DELETE 下方无更旧数据、且无活跃旧 snapshot，压实后安全回收".to_string(),
                });
            }
        }

        if !kept.is_empty() {
            groups.push(KeyGroup { key, entries: kept });
        }
    }

    // 2. 范围删除去重 + 保留判定。
    let mut seen: BTreeMap<String, RangeFragment> = BTreeMap::new();
    for file in &inputs {
        for frag in &file.range_fragments {
            seen.entry(frag.name.clone()).or_insert_with(|| frag.clone());
        }
    }

    let mut fragments_out: Vec<(RangeFragment, Vec<RangeRetentionNote>)> = Vec::new();
    for (_, frag) in seen {
        let mut notes = Vec::new();
        if let Some(snap) = oldest {
            if snap.seq >= frag.seq {
                notes.push(RangeRetentionNote {
                    fragment_id: frag.fragment_id.clone(),
                    reason_code: "range_keep_active_snapshot".to_string(),
                    detail: format!(
                        "活跃旧 snapshot {} 持有序号 {} >= tombstone seq {}，保守保留以防覆盖区间内的旧读路径",
                        snap.id, snap.seq, frag.seq
                    ),
                });
            }
        }
        let mut covered_external: Vec<String> = Vec::new();
        for deep in &deeper {
            if deep.overlaps(&frag.begin, &frag.end_exclusive) {
                for entry in &deep.entries {
                    if entry.op == OpType::Put
                        && entry.seq < frag.seq
                        && entry.key >= frag.begin
                        && entry.key < frag.end_exclusive
                    {
                        covered_external.push(format!("{}#L{}({})", deep.id, deep.level, entry.key));
                    }
                }
            }
        }
        if !covered_external.is_empty() {
            notes.push(RangeRetentionNote {
                fragment_id: frag.fragment_id.clone(),
                reason_code: "range_keep_older_data_outside".to_string(),
                detail: format!(
                    "未参与压实的更深层文件仍含区间 [{}, {}) 内更旧点数据 {}，tombstone 必须下沉，不能提前丢弃",
                    frag.begin, frag.end_exclusive, covered_external.join(", ")
                ),
            });
        }
        if !notes.is_empty() {
            fragments_out.push((frag, notes));
        } else {
            drops.push(DropReason {
                entry_id: format!("range:{}@{}", frag.name, frag.seq),
                key: frag.begin.clone(),
                seq: frag.seq,
                op: OpType::RangeDelBegin,
                reason_code: "range_tombstone_dropped_safe".to_string(),
                detail: format!(
                    "范围删除 [{}, {}) 下方无更旧数据、无活跃旧 snapshot，压实后安全回收",
                    frag.begin, frag.end_exclusive
                ),
            });
        }
    }

    MergeArtifacts { groups, drops, fragments: fragments_out }
}

fn split_outputs(
    job_name: &str,
    artifacts: MergeArtifacts,
    params: &Params,
) -> LoadResult<(
    Vec<(Vec<KeyGroup>, Vec<(RangeFragment, Vec<RangeRetentionNote>)>)>,
    Vec<SplitBoundary>,
    Vec<DropReason>,
)> {
    let drops = artifacts.drops.clone();
    let budget = &params.budget;
    let cap = budget.max_entries_per_output;

    let mut boundaries: BTreeMap<String, ()> = BTreeMap::new();
    for group in &artifacts.groups {
        boundaries.insert(group.key.clone(), ());
    }
    for (frag, _) in &artifacts.fragments {
        boundaries.insert(frag.begin.clone(), ());
        boundaries.insert(frag.end_exclusive.clone(), ());
    }
    let _ = &drops;

    // 贪心装箱，仅在 key 边界切。
    let mut packed: Vec<Vec<KeyGroup>> = vec![Vec::new()];
    let mut counts = vec![0u32];
    for group in artifacts.groups {
        let n = group.entries.len() as u32;
        if n > cap {
            return Err(LoadError::BudgetExhausted {
                job: job_name.to_string(),
                needed_entries: n,
                capacity_entries: cap,
                needed_outputs: 1,
                max_outputs: budget.max_outputs,
                pos: crate::domain::errors::DiagnosticPos {
                    path: format!("compaction.{job_name}.key.{}", group.key),
                    detail: format!(
                        "单个 key {} 的版本链有 {n} 条，超过单输出容量 {cap}，且不允许在 key 内切分",
                        group.key
                    ),
                },
            });
        }
        let last = packed.len() - 1;
        if counts[last] + n > cap && !packed[last].is_empty() {
            packed.push(Vec::new());
            counts.push(0);
        }
        let last = packed.len() - 1;
        counts[last] += n;
        packed.last_mut().unwrap().push(group);
    }
    if packed.len() as u32 > budget.max_outputs {
        return Err(LoadError::BudgetExhausted {
            job: job_name.to_string(),
            needed_outputs: packed.len() as u32,
            max_outputs: budget.max_outputs,
            needed_entries: counts.iter().sum(),
            capacity_entries: cap * budget.max_outputs,
            pos: crate::domain::errors::DiagnosticPos {
                path: format!("compaction.{job_name}.outputs"),
                detail: format!("预算只允许 {} 个输出，至少需要 {} 个", budget.max_outputs, packed.len()),
            },
        });
    }

    let mut split_notes = Vec::new();
    let mut out_groups: Vec<(Vec<KeyGroup>, Vec<(RangeFragment, Vec<RangeRetentionNote>)>)> = Vec::new();
    let total = packed.len();
    for (idx, groups) in packed.into_iter().enumerate() {
        if groups.is_empty() {
            continue;
        }
        let smallest = groups.first().unwrap().key.clone();
        let largest = groups.last().unwrap().key.clone();

        // 保留的范围片段：裁剪/复制进与其区间相交的输出。
        let mut frags_here: Vec<(RangeFragment, Vec<RangeRetentionNote>)> = Vec::new();
        for (frag, notes) in &artifacts.fragments {
            let intersects = groups.iter().any(|g| g.key >= frag.begin && g.key < frag.end_exclusive);
            if intersects {
                let begin = if frag.begin > smallest { frag.begin.clone() } else { smallest.clone() };
                let successor = boundaries
                    .range((std::ops::Bound::Excluded(largest.clone()), std::ops::Bound::Unbounded))
                    .next()
                    .map(|(k, _)| k.clone());
                let end = match &successor {
                    Some(next) if *next < frag.end_exclusive => next.clone(),
                    _ => frag.end_exclusive.clone(),
                };
                let mut clipped = frag.clone();
                clipped.begin = begin;
                clipped.end_exclusive = end;
                clipped.fragment_id = fragment_id(&frag.name, frag.seq, &clipped.begin, &clipped.end_exclusive, &frag.origin_wal);
                clipped.retention = notes.iter().map(|n| n.reason_code.clone()).collect();
                frags_here.push((clipped, notes.clone()));
            }
        }

        if idx + 1 < total {
            split_notes.push(SplitBoundary {
                index: idx as u32,
                after_key: largest.clone(),
                next_key: String::new(),
                entries_before: groups.iter().map(|g| g.entries.len() as u32).sum(),
                constraint: "split_at_key_boundary".to_string(),
            });
        }
        out_groups.push((groups, frags_here));
    }

    // 修正 split note 的 next_key。
    let keys: Vec<String> = out_groups.iter().map(|(g, _)| g.first().unwrap().key.clone()).collect();
    for (i, note) in split_notes.iter_mut().enumerate() {
        note.next_key = keys[i + 1].clone();
    }

    Ok((out_groups, split_notes, drops))
}

/// 计算一次压实（不安装）。输出层同层文件必须互不重叠，否则判 SemanticUnresolved。
pub fn run_compaction(
    job_name: &str,
    files: &[SstFile],
    snapshots: &[Snapshot],
    params: &Params,
    seed_id: &str,
    wal_ids: &[String],
) -> LoadResult<CompactionResult> {
    let pick = pick_inputs(files, seed_id)?;
    let artifacts = merge(files, &pick, snapshots);
    let (out_groups, split_notes, drops) = split_outputs(job_name, artifacts, params)?;

    let mut outputs: Vec<OutputSummary> = Vec::new();
    let mut all_retention: Vec<RangeRetentionNote> = Vec::new();
    let mut prev_largest: Option<String> = None;
    for (idx, (groups, frags)) in out_groups.into_iter().enumerate() {
        let entries: Vec<Entry> = groups.iter().flat_map(|g| g.entries.iter().cloned()).collect();
        let smallest = entries.first().map(|e| e.key.clone()).unwrap_or_default();
        let largest = entries.last().map(|e| e.key.clone()).unwrap_or_default();

        if let Some(prev) = &prev_largest {
            if *prev >= smallest {
                return Err(LoadError::SemanticUnresolved {
                    pos: crate::domain::errors::DiagnosticPos {
                        path: format!("compaction.{job_name}.outputs[{idx}]"),
                        detail: format!(
                            "输出切分违反不重叠层约束：上一输出最大键 {prev} >= 本输出最小键 {smallest}"
                        ),
                    },
                    candidates: vec![
                        serde_json::json!({"fix": "move_boundary", "after_key": prev}),
                        serde_json::json!({"fix": "merge_outputs", "index": idx}),
                    ],
                    diagnostics: vec!["同层输出必须互不重叠；候选修复方案已保留".to_string()],
                });
            }
        }
        prev_largest = Some(largest.clone());

        let range_fragments: Vec<RangeFragment> = frags.iter().map(|(f, _)| f.clone()).collect();
        for (_, notes) in &frags {
            all_retention.extend(notes.clone());
        }
        let file_seed = serde_json::json!({
            "job": job_name,
            "idx": idx,
            "entries": entries.iter().map(|e| &e.entry_id).collect::<Vec<_>>(),
            "frags": range_fragments.iter().map(|f| &f.fragment_id).collect::<Vec<_>>(),
        });
        let file_id = stable_id("sst", &file_seed);
        outputs.push(OutputSummary {
            file_id,
            level: pick.target_level,
            smallest,
            largest,
            end_inclusive: true,
            entry_count: entries.len() as u32,
            entries,
            range_fragments,
            drops: Vec::new(),
            split: split_notes.iter().find(|n| n.index == idx as u32).cloned(),
        });
    }

    // 丢弃记录挂在首个输出摘要上（压实整体记录）。
    if let Some(first) = outputs.first_mut() {
        first.drops = drops;
    }

    let result_seed = serde_json::json!({
        "job": job_name,
        "inputs": pick.inputs,
        "outputs": outputs.iter().map(|o| &o.file_id).collect::<Vec<_>>(),
        "params": params.version,
    });
    let id = stable_id("cmp", &result_seed);

    Ok(CompactionResult {
        id,
        job_name: job_name.to_string(),
        target_level: pick.target_level,
        inputs: pick.inputs.clone(),
        pick_reasons: pick.reasons.clone(),
        outputs,
        range_retention: all_retention,
        provenance: Provenance {
            input_files: pick.inputs.clone(),
            input_wal: wal_ids.to_vec(),
            snapshots: snapshots.iter().filter(|s| s.active).map(|s| s.id.clone()).collect(),
            params_version: params.version.clone(),
            parents: Vec::new(),
        },
        status: CompactionStatus::Proposed,
        installed_by: None,
        candidates: Vec::new(),
        diagnostics: Vec::new(),
    })
}
