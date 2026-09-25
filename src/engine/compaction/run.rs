//! compact() 主流程。

use super::*;
use std::collections::BTreeSet;

/// 执行一次压实。纯函数：输入为当前仍生效的 SSTable、snapshot 与参数版本。
#[allow(clippy::too_many_arguments)]
pub fn compact(
    req: &CompactionRequest,
    params: &CompactionParams,
    live_ssts: &[SSTable],
    snapshots: &[Snapshot],
) -> CompactionOutcome {
    match run(req, params, live_ssts, snapshots) {
        Ok(report) => CompactionOutcome::Report { report },
        Err(AppError::ResourceExhausted { diagnostic }) => CompactionOutcome::ResourceExhausted {
            code: diagnostic.code,
            message: diagnostic.message,
            req_id: req.id.clone(),
        },
        Err(AppError::Corrupt { diagnostic }) => CompactionOutcome::Corrupt {
            code: diagnostic.code,
            message: diagnostic.message,
            req_id: req.id.clone(),
        },
        Err(AppError::SemanticUnsolvable { diagnostic, candidates }) => {
            CompactionOutcome::SemanticUnsolvable {
                code: diagnostic.code,
                message: diagnostic.message,
                req_id: req.id.clone(),
                candidates,
                evidence: Vec::new(),
            }
        }
        Err(other) => CompactionOutcome::Corrupt {
            code: other.code().to_string(),
            message: other.to_string(),
            req_id: req.id.clone(),
        },
    }
}

fn run(
    req: &CompactionRequest,
    params: &CompactionParams,
    live_ssts: &[SSTable],
    snapshots: &[Snapshot],
) -> Result<CompactionReport, AppError> {
    let mut evidence: Vec<EvidenceEvent> = Vec::new();
    let mut selection: Vec<SelectionReason> = Vec::new();

    if req.input_sst_ids.is_empty() {
        return Err(AppError::incomplete(
            &req_path(&req.id, "input_sst_ids"),
            "D_EMPTY_INPUT",
            "压实输入为空，不能用默认输入伪装成功",
        ));
    }

    // 1) 解析输入
    let mut ssts: Vec<&SSTable> = Vec::new();
    for id in &req.input_sst_ids {
        let sst = live_ssts.iter().find(|s| &s.id == id).ok_or_else(|| {
            AppError::incomplete(
                &req_path(&req.id, "input_sst_ids"),
                "D_INPUT_NOT_LIVE",
                format!("输入 SSTable {id} 不在当前生效集合中（可能已被先前压实取代）"),
            )
        })?;
        if !sst.diagnostics.is_empty() {
            return Err(diag(
                &req_path(&req.id, "input"),
                id,
                "D_CORRUPT_SST_DIAGNOSTIC",
                format!("SSTable {id} 带格式诊断 {:?}，不能参与压实", sst.diagnostics),
            ));
        }
        ssts.push(sst);
    }

    // 2) 层与输出层
    let levels: BTreeSet<i32> = ssts.iter().map(|s| s.level()).collect();
    if levels.is_empty() {
        return Err(AppError::incomplete(&req_path(&req.id, "inputs"), "D_NO_LEVEL", "无输入层"));
    }
    let min_level = *levels.iter().next().unwrap();
    let max_level = *levels.iter().last().unwrap();
    let output_level = match req.output_level {
        Some(l) => l,
        None => {
            if min_level == 0 && max_level > 0 {
                return Err(AppError::SemanticUnsolvable {
                    diagnostic: Diagnostic {
                        code: "D_MIXED_LEVEL_COMPACTION".into(),
                        message: "输入同时跨越 L0 与 L>=1，本案例包不允许该混合压实".into(),
                        pos: Pos { path: req_path(&req.id, "input_sst_ids"), object_id: Some(req.id.clone()) },
                    },
                    candidates: vec![],
                });
            }
            min_level + 1
        }
    };
    if output_level <= max_level {
        return Err(AppError::SemanticUnsolvable {
            diagnostic: Diagnostic {
                code: "D_OUTPUT_LEVEL_NOT_BELOW".into(),
                message: format!("输出层 L{output_level} 必须严格低于最高输入层 L{max_level}（L0 重叠压实除外，其输出固定 L1）"),
                pos: Pos { path: req_path(&req.id, "output_level"), object_id: Some(req.id.clone()) },
            },
            candidates: vec![],
        });
    }

    // 3) 选择理由 + L0 重叠扩展
    let mut chosen_ids: BTreeSet<String> = ssts.iter().map(|s| s.id.clone()).collect();
    for s in &ssts {
        if s.level() == 0 {
            selection.push(SelectionReason {
                code: "SEL_L0_OVERLAPPING".into(),
                message: format!("{} 位于 L0；L0 文件可重叠，压实必须读入全部与其键范围重叠的 L0 文件", s.id),
                related: ssts.iter().filter(|o| o.id != s.id && s.overlaps(o)).map(|o| o.id.clone()).collect(),
            });
        }
    }
    // 传递闭包：重叠的 L0 文件全部加入
    loop {
        let mut added = Vec::new();
        for cand in live_ssts.iter().filter(|s| s.level() == 0 && !chosen_ids.contains(&s.id)) {
            if live_ssts
                .iter()
                .filter(|s| chosen_ids.contains(&s.id) && s.level() == 0)
                .any(|s| s.overlaps(cand))
            {
                added.push(cand.id.clone());
            }
        }
        if added.is_empty() {
            break;
        }
        for id in &added {
            selection.push(SelectionReason {
                code: "SEL_L0_TRANSITIVE_PULL_IN".into(),
                message: format!("{id} 与已选 L0 文件键范围重叠（重叠 L0 闭包），一并拉入压实"),
                related: vec![],
            });
        }
        chosen_ids.extend(added);
    }
    // L>=1：拉入输出层上与输入范围重叠的全部文件，保证输出后该层仍不重叠
    let input_span = key_span(&ssts);
    for cand in live_ssts.iter().filter(|s| s.level() == output_level && !chosen_ids.contains(&s.id)) {
        if span_overlaps(&input_span, cand) {
            chosen_ids.insert(cand.id.clone());
            selection.push(SelectionReason {
                code: "SEL_OUTPUT_LEVEL_OVERLAP".into(),
                message: format!(
                    "{} 位于输出层 L{output_level} 且与输入键范围重叠；必须一并参与，否则输出会破坏该层不重叠约束",
                    cand.id
                ),
                related: vec![],
            });
        }
    }

    let chosen: Vec<&SSTable> = chosen_ids
        .iter()
        .map(|id| *live_ssts.iter().find(|s| &s.id == id).unwrap())
        .collect();

    // 预算前置：输出层参与文件大小不可解释 => 资源耗尽（原始值保留）
    check_sst_sizes_usable(&chosen, &req.id)?;

    // 4) snapshot
    let oldest_active = snapshots
        .iter()
        .filter(|s| s.active)
        .filter_map(|s| s.seqno.normalized)
        .min();
    evidence.push(EvidenceEvent {
        code: "E_OLDEST_ACTIVE_SNAPSHOT".into(),
        message: match oldest_active {
            Some(s) => format!("最旧活跃 snapshot = seq {s}；seqno < {s} 的记录必须保留，旧 snapshot 仍可能读到"),
            None => "无活跃 snapshot；下沉丢弃仅需检查未参与压实的旧数据".into(),
        },
        related: snapshots.iter().filter(|s| s.active).map(|s| s.id.clone()).collect(),
    });

    // 5) 未参与压实的“更深层旧数据”（阻断 tombstone 下沉）
    let chosen_set = &chosen_ids;
    let deeper: Vec<&SSTable> = live_ssts
        .iter()
        .filter(|s| !chosen_set.contains(&s.id) && s.level() >= output_level)
        .collect();

    // 6) 收集 point / range
    let mut points: Vec<&PointRecord> = Vec::new();
    let mut ranges: Vec<&RangeTomb> = Vec::new();
    for s in &chosen {
        points.extend(s.points.iter());
        ranges.extend(s.ranges.iter());
    }

    // 7) 同序号多类型检测（point-point）
    for i in 0..points.len() {
        for j in (i + 1)..points.len() {
            if same_seq_conflict(points[i], points[j]) {
                match params.same_seq_policy {
                    SameSeqPolicy::Ambiguous => {
                        let candidates = vec![
                            format!("put:{}", points[i].id),
                            format!("delete:{}", points[j].id),
                            format!("put:{}", points[j].id),
                            format!("delete:{}", points[i].id),
                        ];
                        evidence.push(EvidenceEvent {
                            code: "E_SAME_SEQ_CONFLICT".into(),
                            message: format!(
                                "键 {} 在 seq {} 同时存在 Put 与 Delete，参数 v{} 策略为不裁决；保留全部合法候选",
                                points[i].key.logical,
                                points[i].seq(),
                                params.version
                            ),
                            related: vec![points[i].id.clone(), points[j].id.clone()],
                        });
                        return Err(AppError::SemanticUnsolvable {
                            diagnostic: Diagnostic {
                                code: "D_SAME_SEQ_MULTI_KIND".into(),
                                message: "同 (user key, seqno) 多类型且无法唯一决定".into(),
                                pos: Pos {
                                    path: req_path(&req.id, "points"),
                                    object_id: Some(points[i].id.clone()),
                                },
                            },
                            candidates,
                        });
                    }
                    SameSeqPolicy::DeleteWins | SameSeqPolicy::PutWins => {
                        evidence.push(EvidenceEvent {
                            code: "E_SAME_SEQ_RESOLVED_BY_PARAM".into(),
                            message: format!(
                                "键 {} 在 seq {} 同序号多类型，按参数 v{} 的 {:?} 规则裁决",
                                points[i].key.logical,
                                points[i].seq(),
                                params.version,
                                params.same_seq_policy
                            ),
                            related: vec![points[i].id.clone(), points[j].id.clone()],
                        });
                    }
                }
            }
        }
    }

    // 内部键顺序排序（等价输入稳定顺序）
    points.sort_by(|a, b| cmp_internal(&a.key.bytes(), a.seq(), a.kind, &b.key.bytes(), b.seq(), b.kind));

    let report = super::merge::build_report(
        req,
        params,
        &chosen,
        &deeper,
        oldest_active,
        points,
        ranges,
        selection,
        evidence,
    )?;
    Ok(report)
}

type Span = (Vec<u8>, Vec<u8>);
fn key_span(ssts: &[&SSTable]) -> Span {
    let mut lo = ssts[0].smallest.bytes();
    let mut hi = ssts[0].largest.bytes();
    for s in &ssts[1..] {
        let a = s.smallest.bytes();
        let b = s.largest.bytes();
        if KeyRep::byte_cmp(&a, &lo) == std::cmp::Ordering::Less {
            lo = a;
        }
        if KeyRep::byte_cmp(&b, &hi) == std::cmp::Ordering::Greater {
            hi = b;
        }
    }
    (lo, hi)
}
fn span_overlaps(span: &Span, s: &SSTable) -> bool {
    !(s.largest.bytes() < span.0 || span.1 < s.smallest.bytes())
}
