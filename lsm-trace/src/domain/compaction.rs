//! 压实（compaction）语义引擎：
//! - 选择理由结构化（重叠 L0 必须一并压实、目标层重叠扩展、预算约束）；
//! - point/range tombstone 的保留与下沉，丢弃必须同时通过 snapshot 安全与未参与文件检查；
//! - 输出按文件大小切分且保持目标层不重叠；
//! - 同序号多类型 => 语义无解，保留全部合法候选。
//!
//! 关键原则（对应需求原文）：
//! “范围删除不能越过未参与压实的旧数据而提前丢弃”：
//!   range tombstone 只有在与之 key 重叠的所有下层文件都参与了本次压实时才可能被丢弃，
//!   且其 seq < oldest_active_snapshot（否则旧 snapshot 仍可能需要它）。
//! “输出切分仍需维持不重叠层约束”：
//!   输出文件之间及与未参与的目标层文件之间必须按 [smallest,largest] 互不相交。

use crate::domain::error::{DResult, DomainError};
use crate::domain::ids::{calc_id, fact_id};
use crate::domain::model::{
    KeySpec, Measure, PointKind, PointRecord, RangeTombstone, RuleSet, Seq, SstFileSpec,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionRequest {
    pub plan_label: String,
    pub target_level: u32,
    /// 用户/案例显式选择的输入文件 cid（L0/L1... 均可）。
    pub seeds: Vec<String>,
    pub active_snapshot_seqs: Vec<Seq>,
    /// 是否真正安装（false => 只产出计划事实，便于审阅）。
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionReason {
    pub code: String, // machine-readable
    pub file_cid: String,
    pub level: u32,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointDecision {
    pub rid: String,
    pub seq: Seq,
    pub outcome: String, // survive_obsolete | survive_latest_put | survive_latest_delete | drop_shadowed | drop_obsolete | drop_bottommost_delete
    pub output_file: Option<String>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeDecision {
    pub rid: String,
    pub seq: Seq,
    pub outcome: String, // retained_cover_bystander | retained_snapshot_guard | dropped_bottommost_safe
    pub output_files: Vec<String>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFile {
    pub cid: String,
    pub level: u32,
    pub smallest_raw: String,
    pub largest_raw: String,
    pub size: Measure,
    pub point_rids: Vec<String>,
    pub range_rids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantCheck {
    pub code: String,
    pub passed: bool,
    pub detail: String,
    #[serde(default)]
    pub offenders: Vec<String>,
}

/// 同序号多类型的人工/上游决定（可修改决定层）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollisionResolution {
    /// key_raw -> 胜出 rid。
    pub pick_rid_by_key: std::collections::BTreeMap<String, String>,
    pub decided_by: String, // decision id
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPlan {
    pub plan_id: String,
    pub calc_id: String,
    pub rule_version: String,
    pub input_digest: String,
    pub request: CompactionRequest,
    pub oldest_active_snapshot: Option<Seq>,
    pub selected_inputs: Vec<String>,
    pub bystanders: Vec<String>,
    pub selection: Vec<SelectionReason>,
    pub outputs: Vec<OutputFile>,
    pub point_decisions: Vec<PointDecision>,
    pub range_decisions: Vec<RangeDecision>,
    pub checks: Vec<InvariantCheck>,
    pub events: Vec<serde_json::Value>,
    /// 语义无解时保留的全部合法候选。
    #[serde(default)]
    pub collision_candidates: Vec<serde_json::Value>,
    pub requires_resolution: bool,
}

fn file_by_cid<'a>(files: &'a [SstFileSpec], cid: &str) -> DResult<&'a SstFileSpec> {
    files
        .iter()
        .find(|f| f.cid == cid)
        .ok_or_else(|| DomainError::FormatCorrupt {
            location: format!("compaction/seeds/{cid}"),
            reason: "引用了不存在的 SST 文件".to_string(),
            raw: cid.to_string(),
        })
}

fn key_of(f: &SstFileSpec, loc: &str) -> DResult<(Vec<u8>, Vec<u8>)> {
    Ok((f.smallest.decode(loc)?, f.largest.decode(loc)?))
}

fn overlaps(a_lo: &[u8], a_hi: &[u8], b_lo: &[u8], b_hi: &[u8]) -> bool {
    !(a_hi < b_lo || b_hi < a_lo)
}

/// 选择压实输入：种子 + 重叠 L0 扩展 + 目标层重叠扩展（受 max_input_files 预算约束）。
pub fn select_inputs<'a>(
    files: &'a [SstFileSpec],
    req: &CompactionRequest,
    rules: &RuleSet,
) -> DResult<(Vec<&'a SstFileSpec>, Vec<SelectionReason>, Vec<&'a SstFileSpec>)> {
    let mut reasons: Vec<SelectionReason> = Vec::new();
    let mut chosen: Vec<&SstFileSpec> = Vec::new();
    let mut reason_codes: std::collections::BTreeSet<String> = Default::default();

    for seed in &req.seeds {
        let f = file_by_cid(files, seed)?;
        if !chosen.iter().any(|c| c.cid == f.cid) {
            chosen.push(f);
            reasons.push(SelectionReason {
                code: "seed".to_string(),
                file_cid: f.cid.clone(),
                level: f.level,
                detail: "显式选择为压实种子".to_string(),
            });
        }
    }

    let mut changed = true;
    while changed {
        changed = false;

        // 1) 重叠 L0：L0 文件互相可重叠，压实时必须拉入所有重叠 L0，否则会破坏读取。
        if rules.expand_overlapping_l0 {
            let all_l0: Vec<&SstFileSpec> = files.iter().filter(|f| f.level == 0).collect();
            let mut added = Vec::new();
            for c in chosen.iter().filter(|f| f.level == 0) {
                let (lo, hi) = key_of(c, &format!("sst:{}/range", c.cid))?;
                for other in &all_l0 {
                    if other.cid == c.cid || chosen.iter().any(|x| x.cid == other.cid) {
                        continue;
                    }
                    let (olo, ohi) = key_of(other, &format!("sst:{}/range", other.cid))?;
                    if overlaps(&lo, &hi, &olo, &ohi) {
                        added.push(*other);
                        reasons.push(SelectionReason {
                            code: "overlapping_l0".to_string(),
                            file_cid: other.cid.clone(),
                            level: 0,
                            detail: format!(
                                "与已选 L0 文件 {} 的 key range 重叠，L0 必须整体压实",
                                c.cid
                            ),
                        });
                    }
                }
            }
            for f in added {
                if chosen.len() >= rules.max_input_files as usize {
                    return Err(DomainError::ResourceExhausted {
                        budget: "max_input_files".to_string(),
                        limit: rules.max_input_files as u64,
                        used: chosen.len() as u64,
                        at: "select_inputs/overlapping_l0".to_string(),
                    });
                }
                chosen.push(f);
                changed = true;
            }
        }

        // 2) 目标层重叠扩展：与当前输入并集 key span 重叠的目标层文件必须参与。
        let targets: Vec<&SstFileSpec> =
            files.iter().filter(|f| f.level == req.target_level && f.level != 0).collect();
        for t in targets {
            if chosen.iter().any(|x| x.cid == t.cid) {
                continue;
            }
            let mut hit = false;
            let (tlo, thi) = key_of(t, &format!("sst:{}/range", t.cid))?;
            for c in &chosen {
                let (lo, hi) = key_of(c, &format!("sst:{}/range", c.cid))?;
                if overlaps(&lo, &hi, &tlo, &thi) {
                    hit = true;
                    break;
                }
            }
            if hit {
                if chosen.len() >= rules.max_input_files as usize {
                    return Err(DomainError::ResourceExhausted {
                        budget: "max_input_files".to_string(),
                        limit: rules.max_input_files as u64,
                        used: chosen.len() as u64,
                        at: "select_inputs/target_level".to_string(),
                    });
                }
                chosen.push(t);
                reasons.push(SelectionReason {
                    code: "target_level_overlap".to_string(),
                    file_cid: t.cid.clone(),
                    level: t.level,
                    detail: "与压实输入 key span 重叠，必须随目标层一起压实，否则会产生重叠输出"
                        .to_string(),
                });
                changed = true;
            }
        }

        for r in &reasons {
            reason_codes.insert(r.code.clone());
        }
        let _ = &mut reason_codes;
    }

    // 确定性顺序：level 升序、cid。
    chosen.sort_by(|a, b| a.level.cmp(&b.level).then_with(|| a.cid.cmp(&b.cid)));
    let chosen_ids: std::collections::BTreeSet<&str> =
        chosen.iter().map(|f| f.cid.as_str()).collect();
    let bystanders: Vec<&SstFileSpec> = files
        .iter()
        .filter(|f| !chosen_ids.contains(f.cid.as_str()))
        .collect();
    Ok((chosen, reasons, bystanders))
}

#[derive(Clone)]
struct PointItem {
    rec: PointRecord,
    key_bytes: Vec<u8>,
    source_cid: String,
    source_level: u32,
}

fn item_bytes(it: &PointItem) -> u64 {
    let kb = it.key_bytes.len() as u64;
    let vb = it
        .rec
        .value
        .as_ref()
        .and_then(|v| v.size.as_ref())
        .and_then(|m| m.normalized_bytes)
        .unwrap_or(0);
    kb + vb + 16 // 16B 内部键固定开销（seq+type）
}

fn collect_points(inputs: &[&SstFileSpec]) -> DResult<Vec<PointItem>> {
    let mut items = Vec::new();
    for f in inputs {
        for r in &f.records {
            let kb = r.key.decode(&format!("sst:{}/record/{}/key", f.cid, r.rid))?;
            items.push(PointItem {
                rec: r.clone(),
                key_bytes: kb,
                source_cid: f.cid.clone(),
                source_level: f.level,
            });
        }
    }
    Ok(items)
}

fn range_bytes(t: &RangeTombstone) -> u64 {
    (t.start.raw.len() + t.end.raw.len()) as u64 + 24
}

/// 主入口：生成结构化压实计划（纯函数，可被测试直接调用）。
pub fn plan_compaction(
    files: &[SstFileSpec],
    rules: &RuleSet,
    req: &CompactionRequest,
    resolution: Option<&CollisionResolution>,
) -> DResult<CompactionPlan> {
    let (inputs, selection, bystanders) = select_inputs(files, req, rules)?;

    let oldest = req.active_snapshot_seqs.iter().min().copied();
    let safe_seq = oldest.unwrap_or(Seq::MAX);

    // 输入摘要（稳定身份来源）：仅依赖输入内容 + 规则版本。
    let input_summary = serde_json::json!({
        "target_level": req.target_level,
        "seeds": req.seeds,
        "snapshots": req.active_snapshot_seqs,
        "inputs": inputs.iter().map(|f| f.cid).collect::<Vec<_>>(),
        "rules_version": rules.version,
    });
    let input_digest = crate::domain::canonical::canonical_hex(&input_summary);
    let cid = calc_id(&rules.version, &input_digest);
    let plan_id = fact_id(&serde_json::json!({"plan": &cid}));

    let mut events = Vec::new();
    events.push(serde_json::json!({
        "code": "oldest_active_snapshot",
        "oldest": oldest,
        "safe_seq": safe_seq,
        "detail": "仅 seq < 该阈值且不被未参与文件约束的删除才可丢弃",
    }));

    // ---- point 归并与判定 ----
    let mut items = collect_points(&inputs)?;
    // 内部键顺序：key 升序、seq 降序、类型序 delete(0) < put(1)。
    items.sort_by(|a, b| {
        a.key_bytes
            .cmp(&b.key_bytes)
            .then_with(|| b.rec.seq.cmp(&a.rec.seq))
            .then_with(|| match (a.rec.kind, b.rec.kind) {
                (PointKind::Delete, PointKind::Put) => std::cmp::Ordering::Less,
                (PointKind::Put, PointKind::Delete) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            })
    });

    // 同序号多类型检测（同一 user key、同一 seq、互斥类型）。
    let mut collisions: Vec<Vec<&PointItem>> = Vec::new();
    let mut candidates = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let mut j = i;
        while j < items.len() && items[j].key_bytes == items[i].key_bytes {
            j += 1;
        }
        // 在该 key 分组内找同 seq 的互斥类型。
        let mut k = i;
        while k < j {
            let seq = items[k].rec.seq;
            let group: Vec<&PointItem> = items[k..j]
                .iter()
                .filter(|x| x.rec.seq == seq)
                .collect();
            let kinds: std::collections::BTreeSet<u8> = group
                .iter()
                .map(|x| match x.rec.kind {
                    PointKind::Delete => 0,
                    PointKind::Put => 1,
                })
                .collect();
            if kinds.len() > 1 {
                let allowed = resolution
                    .and_then(|r| r.pick_rid_by_key.get(&group[0].rec.key.raw))
                    .cloned();
                match allowed {
                    Some(pick) => {
                        events.push(serde_json::json!({
                            "code": "collision_resolved",
                            "key": group[0].rec.key.raw,
                            "seq": seq,
                            "picked_rid": pick,
                            "candidates": group.iter().map(|x| x.rec.rid.clone()).collect::<Vec<_>>(),
                        }));
                    }
                    None => {
                        candidates.push(serde_json::json!({
                            "key_raw": group[0].rec.key.raw,
                            "seq": seq,
                            "legal_candidates": group.iter().map(|x| serde_json::json!({
                                "rid": x.rec.rid, "kind": x.rec.kind,
                                "source_sst_cid": x.source_cid, "source_level": x.source_level
                            })).collect::<Vec<_>>(),
                        }));
                        collisions.push(group);
                    }
                }
            }
            k += 1;
        }
        i = j;
    }

    if !collisions.is_empty() {
        // 语义无解：不能随便挑一个，保留全部合法候选，停止产出输出。
        return Ok(CompactionPlan {
            plan_id,
            calc_id: cid,
            rule_version: rules.version.clone(),
            input_digest,
            request: req.clone(),
            oldest_active_snapshot: oldest,
            selected_inputs: inputs.iter().map(|f| f.cid.clone()).collect(),
            bystanders: bystanders.iter().map(|f| f.cid.clone()).collect(),
            selection,
            outputs: vec![],
            point_decisions: vec![],
            range_decisions: vec![],
            checks: vec![],
            events,
            collision_candidates: candidates,
            requires_resolution: true,
        });
    }

    // bottommost 判定：是否存在比目标层更深且与输入 span 重叠的未参与文件。
    let input_lo = inputs
        .iter()
        .map(|f| f.smallest.decode(&format!("sst:{}/smallest", f.cid)))
        .collect::<DResult<Vec<_>>>()?
        .into_iter()
        .min()
        .unwrap();
    let input_hi = inputs
        .iter()
        .map(|f| f.largest.decode(&format!("sst:{}/largest", f.cid)))
        .collect::<DResult<Vec<_>>>()?
        .into_iter()
        .max()
        .unwrap();
    let deeper_bystander = bystanders.iter().any(|f| {
        f.level > req.target_level
            && key_of(f, "").map(|(lo, hi)| overlaps(&lo, &hi, &input_lo, &input_hi)).unwrap_or(false)
    });
    let bottommost = !deeper_bystander;
    events.push(serde_json::json!({
        "code": "bottommost_check",
        "bottommost": bottommost,
        "detail": if bottommost { "没有更深的重叠未参与文件，delete 可在安全时丢弃" }
                  else { "存在更深的重叠未参与文件，point/range tombstone 必须保留下沉" },
    }));
