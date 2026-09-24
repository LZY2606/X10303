use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Read seq meaning "all committed writes".
pub const LATEST: Seq = Seq::MAX;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Params {
    pub l0_trigger: usize,
    pub output_budget_bytes: usize,
    pub total_budget_bytes: usize,
    pub max_levels: u32,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            l0_trigger: 2,
            output_budget_bytes: 512,
            total_budget_bytes: 1 << 20,
            max_levels: 6,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalLine {
    pub raw: String,
    pub status: String,
    pub diagnostic: Option<String>,
    pub line_no: usize,
    pub seq: Option<Seq>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RejectedLine {
    pub line_no: usize,
    pub raw: String,
    pub diagnostic: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalImportSummary {
    pub parsed: usize,
    pub rejected: Vec<RejectedLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PickReason {
    L0Overlap { count: usize },
    LevelFanout { level: u32 },
    Manual { note: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionPlan {
    pub id: String,
    pub reason: PickReason,
    pub inputs: Vec<String>,
    pub input_levels: Vec<u32>,
    pub target_level: u32,
    pub input_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DropRecord {
    pub subject: String,
    pub reason: String,
    pub basis: String,
    pub guarded_snapshots: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Judgment {
    pub rule: String,
    pub verdict: String,
    pub affected: Vec<String>,
    pub basis: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCompaction {
    pub plan: CompactionPlan,
    pub outputs: Vec<SsTable>,
    pub drops: Vec<DropRecord>,
    pub judgments: Vec<Judgment>,
    pub kept_range_dels: Vec<RangeDel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub id: String,
    pub plan: CompactionPlan,
    pub outputs: Vec<SsTable>,
    pub drops: Vec<DropRecord>,
    pub judgments: Vec<Judgment>,
    pub status: String,
}

/// Three deliberately distinct failure results:
/// - BudgetExhausted: resource budget ran out mid compaction;
/// - Corrupt: a format/sortedness invariant is violated (with location);
/// - Unsolvable: the semantic constraints have no legal outcome.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CompactionError {
    BudgetExhausted { spent: usize, budget: usize },
    Corrupt { location: String, detail: String },
    Unsolvable { detail: String, candidates: Vec<String> },
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactionError::BudgetExhausted { spent, budget } => {
                write!(f, "budget-exhausted: spent {spent} bytes beyond budget {budget}")
            }
            CompactionError::Corrupt { location, detail } => {
                write!(f, "corrupt-format at {location}: {detail}")
            }
            CompactionError::Unsolvable { detail, candidates } => {
                write!(f, "semantic-unsolvable: {detail}; legal candidates={candidates:?}")
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    AlreadyInstalled,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Engine {
    pub tables: BTreeMap<String, SsTable>,
    pub levels: Vec<Vec<String>>,
    pub snapshots: Vec<Snapshot>,
    pub next_seq: Seq,
    pub memtable: Vec<PointEntry>,
    pub memtable_range_dels: Vec<RangeDel>,
    pub wal_raw: Vec<WalLine>,
    pub params: Params,
    pub param_version: u64,
    pub file_counter: u64,
    pub snapshot_counter: u64,
    pub compaction_counter: u64,
    pub pending: BTreeMap<String, PendingCompaction>,
    pub history: Vec<CompactionRecord>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new(Params::default())
    }
}

impl Engine {
    pub fn new(params: Params) -> Self {
        let levels = (0..params.max_levels).map(|_| Vec::new()).collect();
        Engine {
            tables: BTreeMap::new(),
            levels,
            snapshots: Vec::new(),
            next_seq: 1,
            memtable: Vec::new(),
            memtable_range_dels: Vec::new(),
            wal_raw: Vec::new(),
            params,
            param_version: 1,
            file_counter: 0,
            snapshot_counter: 0,
            compaction_counter: 0,
            pending: BTreeMap::new(),
            history: Vec::new(),
        }
    }

    pub fn manifest_digest(&self) -> String {
        let levels: Vec<Vec<String>> = self.levels.iter().map(|lvl| {
            let mut ids: Vec<String> = lvl.iter().cloned().collect();
            ids.sort();
            ids
        }).collect();
        digest_json(&levels)
    }

    fn alloc_file_id(&mut self) -> String {
        self.file_counter += 1;
        format!("sst-{:06}", self.file_counter)
    }

    fn alloc_snapshot_id(&mut self) -> String {
        self.snapshot_counter += 1;
        format!("snap-{:03}", self.snapshot_counter)
    }

    fn alloc_compaction_id(&mut self) -> String {
        self.compactions_counter_safe();
        self.compaction_counter += 1;
        format!("cmp-{:06}", self.compaction_counter)
    }

    fn compactions_counter_safe(&self) {}

    /// Import raw WAL text lines. Unparseable lines are kept in place
    /// (raw representation preserved) with positional diagnostics;
    /// they never silently become defaults.
    pub fn ingest_wal_lines(&mut self, lines: Vec<String>) -> WalImportSummary {
        let mut parsed = 0usize;
        let mut rejected = Vec::new();
        for (idx, raw) in lines.into_iter().enumerate() {
            let line_no = idx + 1;
            let seq = self.next_seq;
            let tokens: Vec<&str> = raw.split_whitespace().collect();
            let accepted = match tokens.as_slice() {
                ["PUT", key, value] => {
                    self.memtable.push(PointEntry::put(key, seq, value));
                    Some(seq)
                }
                ["DEL", key] => {
                    self.memtable.push(PointEntry::delete(key, seq));
                    Some(seq)
                }
                ["RDEL", start, end] if start < end => {
                    self.memtable_range_dels.push(RangeDel {
                        start: (*start).to_string(),
                        end: (*end).to_string(),
                        seq,
                    });
                    Some(seq)
                }
                ["RDEL", _, end_or_eq] => {
                    rejected.push(RejectedLine {
                        line_no,
                        raw: raw.clone(),
                        diagnostic: format!(
                            "invalid range tombstone endpoint at line {line_no}: require RDEL <start> <end> with start < end (byte order), got end={end_or_eq:?}"
                        ),
                    });
                    None
                }
                _ => {
                    rejected.push(RejectedLine {
                        line_no,
                        raw: raw.clone(),
                        diagnostic: format!(
                            "unparseable WAL record at line {line_no}: expected `PUT <k> <v>`, `DEL <k>` or `RDEL <start> <end>`"
                        ),
                    });
                    None
                }
            };
            if let Some(s) = accepted {
                self.next_seq += 1;
                parsed += 1;
                self.wal_raw.push(WalLine {
                    raw,
                    status: "parsed".into(),
                    diagnostic: None,
                    line_no,
                    seq: Some(s),
                });
            } else {
                let diagnostic = rejected.last().map(|r| r.diagnostic.clone());
                self.wal_raw.push(WalLine {
                    raw,
                    status: "rejected".into(),
                    diagnostic,
                    line_no,
                    seq: None,
                });
            }
        }
        WalImportSummary { parsed, rejected }
    }

    /// Inject entries with explicit sequence numbers (used by the
    /// same-sequence-number multi-type counterexample).
    pub fn ingest_raw_entries(&mut self, points: Vec<PointEntry>, range_dels: Vec<RangeDel>) {
        for p in &points {
            self.next_seq = self.next_seq.max(p.key.seq + 1);
        }
        for rd in &range_dels {
            self.next_seq = self.next_seq.max(rd.seq + 1);
        }
        self.memtable.extend(points);
        self.memtable_range_dels.extend(range_dels);
    }

    pub fn flush(&mut self) -> Option<SsTable> {
        if self.memtable.is_empty() && self.memtable_range_dels.is_empty() {
            return None;
        }
        let mut points = std::mem::take(&mut self.memtable);
        points.sort_by(|a, b| InternalKey::cmp_internal(&a.key, &b.key));
        let mut range_dels = std::mem::take(&mut self.memtable_range_dels);
        range_dels.sort_by(|a, b| a.start.cmp(&b.start).then(a.seq.cmp(&b.seq)));
        let table = self.make_table(0, points, range_dels, "memtable-flush".into());
        self.levels[0].push(table.id.clone());
        self.tables.insert(table.id.clone(), table.clone());
        Some(table)
    }

    pub fn create_snapshot(&mut self, name: &str) -> Snapshot {
        self.snapshot_counter += 1;
        let snapshot = Snapshot {
            id: format!("snap-{:03}", self.snapshot_counter),
            name: name.to_string(),
            seq: self.next_seq.saturating_sub(1),
        };
        self.snapshots.push(snapshot.clone());
        snapshot
    }

    fn make_table(
        &mut self,
        level: u32,
        points: Vec<PointEntry>,
        range_dels: Vec<RangeDel>,
        origin: String,
    ) -> SsTable {
        let id = self.alloc_file_id();
        let (smallest, largest) = if let (Some(first), Some(last)) = (points.first(), points.last()) {
            (first.key.clone(), last.key.clone())
        } else {
            let lo = range_dels.iter().map(|r| r.start.clone()).min().unwrap_or_default();
            let hi = range_dels.iter().map(|r| r.end.clone()).max().unwrap_or_default();
            let max_seq = range_dels.iter().map(|r| r.seq).max().unwrap_or(0);
            (
                InternalKey::new(&lo, max_seq, ValueType::Put),
                InternalKey::new(&hi, max_seq, ValueType::Delete),
            )
        };
        let byte_size = points
            .iter()
            .map(|p| p.key.user_key.len() + p.value.as_ref().map(|v| v.len()).unwrap_or(0) + 24)
            .sum::<usize>()
            + range_dels.len() * 40;
        SsTable { id, level, points, range_dels, smallest, largest, byte_size, origin }
    }
}

impl Engine {
    fn plan_digest(ids: &[String], tables: &BTreeMap<String, SsTable>) -> String {
        let summary: Vec<Value> = ids
            .iter()
            .map(|id| {
                let t = &tables[id];
                json!({
                    "id": id,
                    "level": t.level,
                    "points": t.points.len(),
                    "range_dels": t.range_dels.len(),
                    "smallest": t.smallest.label(),
                    "largest": t.largest.label(),
                })
            })
            .collect();
        digest_json(&summary)
    }

    /// Deterministic picker: when L0 holds at least `l0_trigger` files,
    /// compact every L0 file together with all L1 files overlapping the
    /// L0 key span (overlapping L0 must be compacted atomically).
    pub fn pick_compaction(&mut self) -> Option<CompactionPlan> {
        let l0: Vec<String> = self.levels[0].clone();
        if l0.len() >= self.params.l0_trigger {
            let mut lo = String::new();
            let mut hi = String::new();
            for id in &l0 {
                let t = &self.tables[id];
                if lo.is_empty() || t.smallest.user_key < lo {
                    lo = t.smallest.user_key.clone();
                }
                if t.largest.user_key > hi {
                    hi = t.largest.user_key.clone();
                }
            }
            let mut inputs = l0;
            for id in &self.levels[1] {
                let t = &self.tables[id];
                if t.smallest.user_key.as_str() <= hi.as_str()
                    && lo.as_str() <= t.largest.user_key.as_str()
                {
                    inputs.push(id.clone());
                }
            }
            inputs.sort();
            let count = inputs.iter().filter(|id| self.tables[*id].level == 0).count();
            let input_levels = inputs.iter().map(|id| self.tables[id].level).collect();
            let digest = Self::plan_digest(&inputs, &self.tables);
            return Some(CompactionPlan {
                id: self.alloc_compaction_id(),
                reason: PickReason::L0Overlap { count },
                inputs,
                input_levels,
                target_level: 1,
                input_digest: digest,
            });
        }
        // Level fan-out: oversized level n spills one file into n+1.
        for level in 1..self.params.max_levels.saturating_sub(1) {
            if self.levels[level as usize].len() > 4 {
                let pivot = self.levels[level as usize][0].clone();
                let mut inputs = vec![pivot.clone()];
                let span_hi = self.tables[&pivot].largest.user_key.clone();
                let span_lo = self.tables[&pivot].smallest.user_key.clone();
                for id in &self.levels[level as usize + 1] {
                    let t = &self.tables[id];
                    if t.smallest.user_key.as_str() <= span_hi.as_str()
                        && span_lo.as_str() <= t.largest.user_key.as_str()
                    {
                        inputs.push(id.clone());
                    }
                }
                inputs.sort();
                let input_levels = inputs.iter().map(|id| self.tables[id].level).collect();
                let digest = Self::plan_digest(&inputs, &self.tables);
                return Some(CompactionPlan {
                    id: self.alloc_compaction_id(),
                    reason: PickReason::LevelFanout { level },
                    inputs,
                    input_levels,
                    target_level: level + 1,
                    input_digest: digest,
                });
            }
        }
        None
    }

    pub fn manual_plan(
        &mut self,
        inputs: Vec<String>,
        target_level: u32,
        note: &str,
    ) -> Result<CompactionPlan, CompactionError> {
        if target_level >= self.params.max_levels {
            return Err(CompactionError::Unsolvable {
                detail: format!("target level L{target_level} does not exist (max_levels={})", self.params.max_levels),
                candidates: (0..self.params.max_levels).map(|l| format!("L{l}")).collect(),
            });
        }
        for id in &inputs {
            if !self.tables.contains_key(id) {
                return Err(CompactionError::Corrupt {
                    location: id.clone(),
                    detail: "compaction input missing from manifest".into(),
                });
            }
        }
        let mut sorted = inputs;
        sorted.sort();
        sorted.dedup();
        let input_levels = sorted.iter().map(|id| self.tables[id].level).collect();
        let digest = Self::plan_digest(&sorted, &self.tables);
        Ok(CompactionPlan {
            id: self.alloc_compaction_id(),
            reason: PickReason::Manual { note: note.to_string() },
            inputs: sorted,
            input_levels,
            target_level,
            input_digest: digest,
        })
    }
}

impl Engine {
    /// Execute a plan into a pending (not yet installed) compaction.
    /// No manifest change happens here; `install_compaction` applies it.
    pub fn execute_compaction(
        &mut self,
        plan: CompactionPlan,
    ) -> Result<PendingCompaction, CompactionError> {
        let mut input_tables: Vec<SsTable> = Vec::new();
        for id in &plan.inputs {
            match self.tables.get(id) {
                Some(t) => input_tables.push(t.clone()),
                None => {
                    return Err(CompactionError::Corrupt {
                        location: id.clone(),
                        detail: "compaction input missing from manifest".into(),
                    })
                }
            }
        }
        // Format invariants: every input must be sorted by internal key.
        for t in &input_tables {
            for w in t.points.windows(2) {
                if InternalKey::cmp_internal(&w[0].key, &w[1].key) == Ordering::Greater {
                    return Err(CompactionError::Corrupt {
                        location: t.id.clone(),
                        detail: "sstable points violate internal-key order".into(),
                    });
                }
            }
        }

        let mut judgments: Vec<Judgment> = Vec::new();
        let mut drops: Vec<DropRecord> = Vec::new();

        // Merge with a deterministic tie-break (input index, file id order).
        let ordered_inputs: Vec<&String> = plan.inputs.iter().collect();
        let mut merged: Vec<(usize, PointEntry)> = Vec::new();
        for (idx, t) in input_tables.iter().enumerate() {
            for p in &t.points {
                merged.push((idx, p.clone()));
            }
        }
        merged.sort_by(|a, b| {
            InternalKey::cmp_internal(&a.1.key, &b.1.key).then_with(|| a.0.cmp(&b.0))
        });
        let mut deduped: Vec<PointEntry> = Vec::new();
        for (_, p) in merged {
            if let Some(last) = deduped.last() {
                if last.key == p.key {
                    judgments.push(Judgment {
                        rule: "duplicate-internal-key".into(),
                        verdict: "keep-first-by-input-order".into(),
                        affected: vec![p.label()],
                        basis: "equivalent inputs must produce stable output: first occurrence in deterministic input order wins".into(),
                    });
                    continue;
                }
            }
            deduped.push(p);
        }
        for w in deduped.windows(2) {
            if w[0].key.user_key == w[1].key.user_key && w[0].key.seq == w[1].key.seq {
                judgments.push(Judgment {
                    rule: "same-seq-multi-type".into(),
                    verdict: "order-by-kind-desc-put-before-delete".into(),
                    affected: vec![w[0].label(), w[1].label()],
                    basis: "internal comparator: user asc, seq desc, kind desc (put=1 > delete=0); equivalent inputs stay deterministic".into(),
                });
            }
        }

        let snap_seqs: Vec<Seq> = self.snapshots.iter().map(|s| s.seq).collect();
        let input_rds: Vec<RangeDel> =
            input_tables.iter().flat_map(|t| t.range_dels.clone()).collect();

        // Data strictly deeper than the target that does NOT participate:
        // a tombstone may only be elided if nothing there can still be
        // shadowed by it.
        let deeper: Vec<SsTable> = self
            .tables
            .values()
            .filter(|t| t.level > plan.target_level && !plan.inputs.contains(&t.id))
            .cloned()
            .collect();
        let bottommost_key = |k: &str| {
            !deeper.iter().any(|t| {
                t.smallest.user_key.as_str() <= k && k <= t.largest.user_key.as_str()
            })
        };
        let bottommost_range = |lo: &str, hi: &str| {
            !deeper.iter().any(|t| {
                t.smallest.user_key.as_str() < hi && lo < t.largest.user_key.as_str()
            })
        };

        // Snapshot-aware per-user-key retention.
        let mut kept_points: Vec<PointEntry> = Vec::new();
        let mut i = 0;
        while i < deduped.len() {
            let mut j = i;
            while j < deduped.len() && deduped[j].key.user_key == deduped[i].key.user_key {
                j += 1;
            }
            let group = &deduped[i..j];
            let mut group_kept: Vec<PointEntry> = Vec::new();
            for v in group {
                let keep = match group_kept.last() {
                    None => true,
                    Some(last) => {
                        let guards: Vec<Seq> = snap_seqs
                            .iter()
                            .copied()
                            .filter(|&s| v.key.seq <= s && s < last.key.seq)
                            .collect();
                        if guards.is_empty() {
                            drops.push(DropRecord {
                                subject: v.label(),
                                reason: "superseded-by-newer-version".into(),
                                basis: format!(
                                    "shadowed by {} and no active snapshot in [{}, {})",
                                    last.label(),
                                    v.key.seq,
                                    last.key.seq
                                ),
                                guarded_snapshots: vec![],
                            });
                            false
                        } else {
                            judgments.push(Judgment {
                                rule: "snapshot-guards-version".into(),
                                verdict: "retain-superseded-version".into(),
                                affected: vec![v.label()],
                                basis: format!(
                                    "snapshots at seqs {guards:?} still read this version; visibility is decided per snapshot by internal-key order"
                                ),
                            });
                            true
                        }
                    }
                };
                if keep {
                    group_kept.push(v.clone());
                }
            }

            for v in group_kept {
                // Range tombstone coverage, snapshot-guarded.
                let hidden_above: Option<Seq> = input_rds
                    .iter()
                    .filter(|rd| rd.covers(&v.key.user_key, v.key.seq))
                    .map(|rd| rd.seq)
                    .min();
                if let Some(tomb_seq) = hidden_above {
                    let guards: Vec<Seq> = snap_seqs
                        .iter()
                        .copied()
                        .filter(|&s| v.key.seq <= s && s < tomb_seq)
                        .collect();
                    if !guards.is_empty() {
                        judgments.push(Judgment {
                            rule: "snapshot-guards-range-delete".into(),
                            verdict: "keep-covered-version".into(),
                            affected: vec![v.label()],
                            basis: format!(
                                "snapshots at seqs {guards:?} read this version below range tombstone seq {tomb_seq}; it cannot be dropped"
                            ),
                        });
                    } else {
                        drops.push(DropRecord {
                            subject: v.label(),
                            reason: "covered-by-range-tombstone".into(),
                            basis: format!(
                                "covered by range tombstone seq {tomb_seq}; endpoint semantics: [start,end); no snapshot in [entry.seq, tombstone.seq) reads it"
                            ),
                            guarded_snapshots: vec![],
                        });
                        continue;
                    }
                }
                // Point tombstone elision vs sink-down.
                if v.key.kind == ValueType::Delete {
                    if bottommost_key(&v.key.user_key) {
                        drops.push(DropRecord {
                            subject: v.label(),
                            reason: "point-tombstone-elided-bottommost".into(),
                            basis: "no older data exists below the compaction; snapshots below the tombstone retain their versions directly".into(),
                            guarded_snapshots: vec![],
                        });
                        continue;
                    } else {
                        judgments.push(Judgment {
                            rule: "tombstone-sinks".into(),
                            verdict: format!("retain-and-sink-to-L{}", plan.target_level),
                            affected: vec![v.label()],
                            basis: "older data exists in non-participating deeper files; dropping the tombstone early would resurrect it".into(),
                        });
                    }
                }
                kept_points.push(v);
            }
            i = j;
        }

        // Range tombstone retention: never cross non-participating old data.
        let mut kept_rds: Vec<RangeDel> = Vec::new();
        for rd in &input_rds {
            if bottommost_range(&rd.start, &rd.end) {
                drops.push(DropRecord {
                    subject: rd.label(),
                    reason: "range-tombstone-elided-bottommost".into(),
                    basis: "no non-participating deeper file overlaps the range; covered older data was handled in this compaction".into(),
                    guarded_snapshots: vec![],
                });
            } else {
                judgments.push(Judgment {
                    rule: "range-tombstone-retained".into(),
                    verdict: format!("sink-to-L{}", plan.target_level),
                    affected: vec![rd.label()],
                    basis: "range deletion must not be dropped past non-participating older data; it sinks with the output".into(),
                });
                kept_rds.push(rd.clone());
            }
        }

        let outputs = self.split_outputs(
            plan.target_level,
            kept_points,
            &kept_rds,
            &plan.id,
            &mut drops,
            &mut judgments,
        )?;
        self.verify_level_invariant(&plan, &outputs, &mut judgments)?;

        let pending = PendingCompaction {
            plan: plan.clone(),
            outputs: outputs.clone(),
            drops,
            judgments,
            kept_range_dels: kept_rds,
        };
        self.pending.insert(plan.id.clone(), pending.clone());
        Ok(pending)
    }
}

impl Engine {
    fn split_outputs(
        &mut self,
        target_level: u32,
        kept_points: Vec<PointEntry>,
        kept_rds: &[RangeDel],
        plan_id: &str,
        drops: &mut Vec<DropRecord>,
        judgments: &mut Vec<Judgment>,
    ) -> Result<Vec<SsTable>, CompactionError> {
        let budget = self.params.output_budget_bytes;
        let total_budget = self.params.total_budget_bytes;
        let estimate = |p: &PointEntry| {
            p.key.user_key.len() + p.value.as_ref().map(|v| v.len()).unwrap_or(0) + 24
        };

        let mut outputs: Vec<SsTable> = Vec::new();
        let mut current: Vec<PointEntry> = Vec::new();
        let mut current_size = 0usize;
        let mut total = 0usize;

        for p in kept_points {
            let size = estimate(&p);
            total += size;
            if total > total_budget {
                return Err(CompactionError::BudgetExhausted {
                    spent: total,
                    budget: total_budget,
                });
            }
            // Cut only at a user-key boundary: versions of the same user
            // key are never split across output files.
            if !current.is_empty()
                && current_size + size > budget
                && current.last().unwrap().key.user_key != p.key.user_key
            {
                let points = std::mem::take(&mut current);
                let table = self.make_table(
                    target_level,
                    points,
                    rds_for(&kept_rds_iter_safe(kept_rds, &self.tables)),
                    format!("compaction:{plan_id}"),
                );
                outputs.push(table);
                current_size = 0;
            }
            current_size += size;
            current.push(p);
        }
        if !current.is_empty() || (outputs.is_empty() && !kept_rds.is_empty()) {
            // Attach range dels overlapping the file span; if there are no
            // points at all, emit a range-del-only spanning file.
            let span = if current.is_empty() {
                let lo = kept_rds.iter().map(|r| r.start.clone()).min().unwrap();
                let hi = kept_rds.iter().map(|r| r.end.clone()).max().unwrap();
                Some((lo, hi))
            } else {
                None
            };
            let attached: Vec<RangeDel> = if let Some((lo, hi)) = &span {
                kept_rds
                    .iter()
                    .filter(|r| r.overlaps_user_range(lo, hi))
                    .cloned()
                    .collect()
            } else {
                let first_key = current.first().unwrap().key.user_key.clone();
                let last_key = current.last().unwrap().key.user_key.clone();
                kept_rds
                    .iter()
                    .filter(|r| r.overlaps_user_range(&first_key, &last_key_ext(&last_key)))
                    .cloned()
                    .collect()
            };
            let table = self.make_table(
                target_level,
                current,
                attached,
                format!("compaction:{plan_id}"),
            );
            outputs.push(table);
        }
        // Attach overlapping range dels to the point-bearing files as well.
        for table in outputs.iter_mut() {
            if table.origin.ends_with(plan_id) && !table.points.is_empty() {
                let lo = table.smallest.user_key.clone();
                let hi = table.largest.user_key.clone();
                for rd in kept_rds {
                    if rd.overlaps_user_range(&lo, &hi) && !table.range_dels.contains(rd) {
                        table.range_dels.push(rd.clone());
                    }
                }
                table.range_dels.sort_by(|a, b| a.start.cmp(&b.start).then(a.seq.cmp(&b.seq)));
            }
        }
        let _ = drops;

        for t in &outputs {
            judgments.push(Judgment {
                rule: "output-boundary".into(),
                verdict: format!("[{} .. {}]", t.smallest.label(), t.largest.label()),
                affected: vec![t.id.clone()],
                basis: format!(
                    "split budget {budget} bytes; cut only between user keys; size={} bytes",
                    t.byte_size
                ),
            });
        }
        if outputs.len() > 1 {
            judgments.push(Judgment {
                rule: "output-split-non-overlap".into(),
                verdict: "verified".into(),
                affected: outputs.iter().map(|t| t.id.clone()).collect(),
                basis: "L1+ outputs must stay key-disjoint and ordered after splitting".into(),
            });
        }
        Ok(outputs)
    }

    fn verify_level_invariant(
        &self,
        plan: &CompactionPlan,
        outputs: &[SsTable],
        judgments: &mut Vec<Judgment>,
    ) -> Result<(), CompactionError> {
        if plan.target_level == 0 {
            return Ok(());
        }
        let mut spans: Vec<(String, String, String)> = Vec::new();
        for id in &self.levels[plan.target_level as usize] {
            if plan.inputs.contains(id) {
                continue;
            }
            let t = &self.tables[id];
            spans.push((t.smallest.user_key.clone(), t.largest.user_key.clone(), t.id.clone()));
        }
        for t in outputs {
            spans.push((t.smallest.user_key.clone(), t.largest.user_key.clone(), t.id.clone()));
        }
        spans.sort_by(|a, b| a.0.cmp(&b.0));
        for w in spans.windows(2) {
            if w[0].1 >= w[1].0 {
                return Err(CompactionError::Unsolvable {
                    detail: format!(
                        "output split violates non-overlapping level constraint at L{}: {} [..{}] overlaps {} [{}..]",
                        plan.target_level, w[0].2, w[0].1, w[1].2, w[1].0
                    ),
                    candidates: vec![
                        "choose a different split boundary".into(),
                        "compact overlapping files together".into(),
                    ],
                });
            }
        }
        judgments.push(Judgment {
            rule: "target-level-non-overlap".into(),
            verdict: format!("verified against existing L{} files", plan.target_level),
            affected: spans.iter().map(|s| s.2.clone()).collect(),
            basis: "existing files not participating in compaction are retained in the invariant check".into(),
        });
        Ok(())
    }

    pub fn install_compaction(&mut self, id: &str) -> InstallOutcome {
        if self.history.iter().any(|r| r.id == id && r.status == "installed") {
            return InstallOutcome::AlreadyInstalled;
        }
        let Some(pending) = self.pending.remove(id) else {
            return InstallOutcome::Unknown;
        };
        for input in &pending.plan.inputs {
            self.tables.remove(input);
        }
        for level in self.levels.iter_mut() {
            level.retain(|f| !pending.plan.inputs.contains(f));
        }
        for t in &pending.outputs {
            self.tables.insert(t.id.clone(), t.clone());
        }
        let target = pending.plan.target_level as usize;
        self.levels[target].extend(pending.outputs.iter().map(|t| t.id.clone()));
        self.levels[target].sort_by(|a, b| {
            self.tables[a].smallest.user_key.cmp(&self.tables[b].smallest.user_key)
        });
        self.history.push(CompactionRecord {
            id: pending.plan.id.clone(),
            plan: pending.plan,
            outputs: pending.outputs,
            drops: pending.drops,
            judgments: pending.judgments,
            status: "installed".into(),
        });
        InstallOutcome::Installed
    }

    pub fn abort_compaction(&mut self, id: &str) -> Option<CompactionRecord> {
        let pending = self.pending.remove(id)?;
        let record = CompactionRecord {
            id: pending.plan.id.clone(),
            plan: pending.plan,
            outputs: pending.outputs,
            drops: pending.drops,
            judgments: pending.judgments,
            status: "aborted".into(),
        };
        self.history.push(record.clone());
        Some(record)
    }
}

fn last_key_ext(k: &str) -> String {
    k.to_string()
}

#[allow(clippy::needless_pass_by_value)]
fn kept_rds_iter_safe(_rds: &[RangeDel], _tables: &BTreeMap<String, SsTable>) -> Vec<RangeDel> {
    Vec::new()
}

fn rds_for(v: &[RangeDel]) -> Vec<RangeDel> {
    v.to_vec()
}
