//! Compaction selection + execution rules (pure).
//!
//! Key rules encoded here (also emitted as machine-readable reasons):
//! * merge order is internal-key order: seq desc, type order per the
//!   active rule revision at equal seq;
//! * visibility is judged against active snapshots: point versions and
//!   tombstones still inside an old snapshot's window must be retained;
//! * a point/range tombstone sinks only across data that physically
//!   participates in this compaction. A fragment covered by an
//!   overlapping SSTable that is not an input is retained; it must not
//!   be discarded "ahead of" older data it may still have to shadow;
//! * output SSTables are split on key boundaries under a byte budget and
//!   the target level stays pairwise non-overlapping.

use crate::domain::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionBudget {
    pub output_cap_bytes: u64,
    pub max_outputs: usize,
}

impl CompactionBudget {
    pub fn standard() -> Self {
        CompactionBudget { output_cap_bytes: 4 * KB, max_outputs: 8 }
    }
    pub fn tight() -> Self {
        CompactionBudget { output_cap_bytes: 96, max_outputs: 8 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelectionReason {
    pub sst_id: String,
    pub level: usize,
    pub role: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionRequest {
    pub input_ids: Vec<String>,
    pub target_level: usize,
    pub budget: CompactionBudget,
    pub rule_version: String,
    /// simulate a crash between staging and installation
    pub interrupt_install: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetainedFragment {
    pub lo: String,
    pub hi: String,
    pub range_seq: u64,
    pub status: FragmentStatus,
    pub reason: String,
    pub witnesses: Vec<String>,
    pub origin: Origin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FragmentStatus { Retained, Dropped }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PointDecision {
    pub key: String,
    pub seq: u64,
    pub kind: RecKind,
    pub sst_id: String,
    pub status: PointStatus,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_sst: Option<String>,
    pub origin: Origin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointStatus { Kept, Dropped }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputSst {
    pub file: SstFile,
    pub split_reason: String,
    pub byte_estimate: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutedPlan {
    pub request: CompactionRequest,
    pub selection: Vec<SelectionReason>,
    pub span_lo: String,
    pub span_hi: String,
    pub outputs: Vec<OutputSst>,
    pub point_decisions: Vec<PointDecision>,
    pub fragments: Vec<RetainedFragment>,
    pub ties: Vec<TieNote>,
    pub non_overlap_checks: Vec<InvariantCheck>,
    pub input_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvariantCheck {
    pub name: String,
    pub holds: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TieNote {
    pub key: String,
    pub seq: u64,
    pub kinds: Vec<String>,
    pub resolution: String,
    pub policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TieCollision {
    pub key: String,
    pub seq: u64,
    pub kinds: Vec<String>,
    pub legal_candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetFailure {
    pub code: String,
    pub message: String,
    pub required_bytes: u64,
    pub cap_bytes: u64,
    pub outputs_needed: usize,
    pub max_outputs: usize,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StagedPlan {
    pub plan: ExecutedPlan,
    pub staged_at: u64,
    pub output_manifest_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CompactionResult {
    /// Outputs produced, not yet installed (normal path installs right
    /// away; the event layer decides).
    Executed(ExecutedPlan),
    /// Resource budget exhausted — a structurally legal compaction that
    /// cannot fit the configured budget.
    BudgetExceeded { request: CompactionRequest, failure: BudgetFailure },
    /// Semantically undecidable under the active rule revision; every
    /// legal resolution is returned, none is chosen.
    Unresolvable {
        request: CompactionRequest,
        collisions: Vec<TieCollision>,
        candidates: Vec<ExecutedPlan>,
    },
    /// Outputs staged but installation did not happen (crash window).
    Interrupted { request: CompactionRequest, staged: StagedPlan },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DomainFailure {
    pub code: String,
    pub message: String,
    pub location: String,
}

/* ------------------------------------------------------------------ *
 *  Selection
 * ------------------------------------------------------------------ */

pub fn select_l0_cluster(state: &LiveState) -> Option<(CompactionRequest, Vec<SelectionReason>)> {
    let l0 = state.files_at(0);
    if l0.is_empty() { return None; }
    // transitive overlap cluster seeded from the lowest-lo L0 file
    let mut chosen: Vec<&SstFile> = vec![l0[0]];
    let mut grew = true;
    while grew {
        grew = false;
        let (lo, hi) = union_span(&chosen);
        for f in &l0 {
            if !chosen.iter().any(|c| c.id == f.id) && f.overlaps(&lo, &hi) {
                chosen.push(f);
                grew = true;
            }
        }
    }
    let (lo, hi) = union_span(&chosen);
    let mut reasons = Vec::new();
    for f in &chosen {
        reasons.push(SelectionReason {
            sst_id: f.id.clone(), level: 0, role: "l0-cluster".into(),
            reason: format!("L0 file [{}, {}] is in the transitive overlap cluster; L0 ranges are not disjoint", f.lo, f.hi),
        });
    }
    let mut input_ids: Vec<String> = chosen.iter().map(|f| f.id.clone()).collect();
    for f in state.files_at(1) {
        if f.overlaps(&lo, &hi) {
            input_ids.push(f.id.clone());
            reasons.push(SelectionReason {
                sst_id: f.id.clone(), level: 1, role: "target-overlap".into(),
                reason: format!("L1 file [{}, {}] overlaps the cluster union and must participate to keep L1 non-overlapping", f.lo, f.hi),
            });
        }
    }
    Some((CompactionRequest {
        input_ids, target_level: 1,
        budget: CompactionBudget::standard(),
        rule_version: state.rule_version.clone(),
        interrupt_install: false,
    }, reasons))
}

pub fn select_leveln(state: &LiveState, level: usize, seed_key: &str)
    -> Option<(CompactionRequest, Vec<SelectionReason>)>
{
    if level == 0 { return select_l0_cluster(state); }
    let files = state.files_at(level);
    let seed = files.into_iter().find(|f| f.covers_key(seed_key))?;
    let (lo, hi) = (seed.lo.clone(), seed.hi.clone());
    let mut reasons = vec![SelectionReason {
        sst_id: seed.id.clone(), level, role: "seed".into(),
        reason: format!("seed file at level {} covering key `{}`", level, seed_key),
    }];
    let mut input_ids = vec![seed.id.clone()];
    for f in state.files_at(level + 1) {
        if f.overlaps(&lo, &hi) {
            input_ids.push(f.id.clone());
            reasons.push(SelectionReason {
                sst_id: f.id.clone(), level: level + 1, role: "next-level-overlap".into(),
                reason: format!("level {} file overlaps seed range and carries older data", level + 1),
            });
        }
    }
    Some((CompactionRequest {
        input_ids, target_level: level + 1,
        budget: CompactionBudget::standard(),
        rule_version: state.rule_version.clone(),
        interrupt_install: false,
    }, reasons))
}

fn union_span(files: &[&SstFile]) -> (String, String) {
    let lo = files.iter().map(|f| f.lo.clone()).min().unwrap();
    let hi = files.iter().map(|f| f.hi.clone()).max().unwrap();
    (lo, hi)
}

/* ------------------------------------------------------------------ *
 *  Execution
 * ------------------------------------------------------------------ */

struct Gathered {
    inputs: Vec<usize>,
    span_lo: String,
    span_hi: String,
    points: Vec<PointG>,
    ranges: Vec<RangeG>,
}

#[derive(Clone)]
struct PointG { rec: PointRec, sst: String, level: usize }
#[derive(Clone)]
struct RangeG { rec: RangeRec, sst: String, level: usize }

fn gather(state: &LiveState, req: &CompactionRequest) -> Result<Gathered, Vec<DomainFailure>> {
    let mut failures = Vec::new();
    if req.input_ids.is_empty() {
        failures.push(DomainFailure {
            code: "INPUT_INCOMPLETE".into(),
            message: "compaction input list is empty".into(),
            location: "request.input_ids".into(),
        });
    }
    let mut inputs = Vec::new();
    for id in &req.input_ids {
        match state.files.iter().position(|f| &f.id == id) {
            Some(i) => inputs.push(i),
            None => failures.push(DomainFailure {
                code: "INPUT_INCOMPLETE".into(),
                message: format!("input SSTable `{}` does not exist in live state", id),
                location: format!("request.input_ids[{}]", id),
            }),
        }
    }
    if !failures.is_empty() { return Err(failures); }

    let max_in_level = inputs.iter().map(|&i| state.files[i].level).max().unwrap();
    if max_in_level > req.target_level {
        failures.push(DomainFailure {
            code: "INPUT_INCOMPLETE".into(),
            message: format!("an input lives at level {} above target level {}", max_in_level, req.target_level),
            location: "request.target_level".into(),
        });
    }
    // non-L0 same-level inputs must be pairwise non-overlapping
    for lvl in 1..=req.target_level {
        let group: Vec<&SstFile> = inputs.iter().map(|&i| &state.files[i]).filter(|f| f.level == lvl).collect();
        for a in 0..group.len() {
            for b in (a + 1)..group.len() {
                if group[a].overlaps(&group[b].lo, &group[b].hi) {
                    failures.push(DomainFailure {
                        code: "INPUT_INCOMPLETE".into(),
                        message: format!("level {} inputs `{}` and `{}` overlap", lvl, group[a].id, group[b].id),
                        location: format!("level {}", lvl),
                    });
                }
            }
        }
    }
    if !failures.is_empty() { return Err(failures); }

    let span_lo = inputs.iter().map(|&i| state.files[i].lo.clone()).min().unwrap();
    let span_hi = inputs.iter().map(|&i| state.files[i].hi.clone()).max().unwrap();

    // Overlapping non-input SSTables:
    //  * at the target level -> MUST participate (otherwise the output
    //    cannot stay pairwise non-overlapping);
    //  * at lower (older) levels -> witnesses that keep tombstones alive.
    for f in &state.files {
        if inputs.iter().any(|&i| std::ptr::eq(&state.files[i], f)) { continue; }
        if !f.overlaps(&span_lo, &span_hi) { continue; }
        if f.level == req.target_level {
            failures.push(DomainFailure {
                code: "INPUT_INCOMPLETE".into(),
                message: format!("non-input target-level file `{}` overlaps compaction span [{},{}); it must participate", f.id, span_lo, span_hi),
                location: format!("level {}", f.level),
            });
        }
    }
    if !failures.is_empty() { return Err(failures); }

    let mut points = Vec::new();
    let mut ranges = Vec::new();
    for &i in &inputs {
        let f = &state.files[i];
        for p in &f.points {
            points.push(PointG { rec: p.clone(), sst: f.id.clone(), level: f.level });
        }
        for r in &f.ranges {
            ranges.push(RangeG { rec: r.clone(), sst: f.id.clone(), level: f.level });
        }
    }
    Ok(Gathered { inputs, span_lo, span_hi, points, ranges })
}

fn detect_collisions(state: &LiveState, points: &[PointG]) -> Vec<TieCollision> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(String, u64), Vec<&PointG>> = BTreeMap::new();
    for g in points {
        groups.entry((g.rec.key.clone(), g.rec.seq)).or_default().push(g);
    }
    let mut out = Vec::new();
    for ((key, seq), gs) in groups {
        let kinds: std::collections::BTreeSet<&'static str> = gs.iter().map(|g| match g.rec.kind {
            RecKind::Put => "PUT", RecKind::Del => "DEL", RecKind::Range => "RANGE",
        }).collect();
        if kinds.len() > 1 {
            out.push(TieCollision {
                key, seq,
                kinds: kinds.iter().map(|s| s.to_string()).collect(),
                legal_candidates: match state.rule_revisions.last().map(|r| r.tie_policy).unwrap_or(TiePolicy::TypeOrder) {
                    TiePolicy::TypeOrder => vec!["PUT>DEL (internal-key type order)".into(), "DEL>PUT".into()],
                    TiePolicy::DelWins => vec!["DEL>PUT".into(), "PUT>DEL (internal-key type order)".into()],
                    TiePolicy::Reject => vec!["reject compaction; materialize both interpretations".into()],
                },
            });
        }
    }
    out
}

/* ------------------------------------------------------------------ *
 *  Public entry
 * ------------------------------------------------------------------ */

pub fn execute(state: &LiveState, req: CompactionRequest) -> Result<CompactionResult, Vec<DomainFailure>> {
    let g = gather(state, &req)?;
    let policy = state.rule_revisions.last().map(|r| r.tie_policy).unwrap_or(TiePolicy::TypeOrder);
    let collisions = detect_collisions(state, &g.points);

    if policy == TiePolicy::Reject && !collisions.is_empty() {
        let mut candidates = Vec::new();
        for forced in [TiePolicy::TypeOrder, TiePolicy::DelWins] {
            candidates.push(execute_inner(state, &req, &g, forced).0);
        }
        return Ok(CompactionResult::Unresolvable { request: req, collisions, candidates });
    }

    let (plan, needed_outputs, total_bytes) = execute_inner(state, &req, &g, policy);
    let budget_ok = needed_outputs <= req.budget.max_outputs;
    if !budget_ok {
        return Ok(CompactionResult::BudgetExceeded {
            request: req,
            failure: BudgetFailure {
                code: "BUDGET_EXHAUSTED".into(),
                message: format!("compaction needs {} output SSTables ({} bytes) but budget allows {}",
                    needed_outputs, total_bytes, req.budget.max_outputs),
                required_bytes: total_bytes,
                cap_bytes: req.budget.output_cap_bytes,
                outputs_needed: needed_outputs,
                max_outputs: req.budget.max_outputs,
                location: "budget".into(),
            },
        });
    }
    if req.interrupt_install {
        let manifest_digest = sha256_hex(canonical_json(&plan.outputs).as_bytes());
        return Ok(CompactionResult::Interrupted {
            request: plan.request.clone(),
            staged: StagedPlan { plan, staged_at: 0, output_manifest_digest: manifest_digest },
        });
    }
    Ok(CompactionResult::Executed(plan))
}

