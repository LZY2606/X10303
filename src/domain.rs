//! Pure LSM domain model: internal-key ordering, snapshot visibility,
//! point/range tombstone retention & sinking during compaction, output
//! splitting and the three distinct failure outcomes.
//!
//! No I/O, no SQLite: the storage and HTTP layers call these functions
//! directly so business rules are unit-testable.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const KB: u64 = 1024;
pub const MB: u64 = 1024 * 1024;

/* ------------------------------------------------------------------ *
 *  Stable identity / canonical hashing
 * ------------------------------------------------------------------ */

pub fn sha256_hex(blob: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(blob);
    hex::encode(h.finalize())
}

/// Canonical JSON (compact, sorted object keys) so equal inputs hash
/// identically across processes / DB round-trips.
pub fn canonical_json<T: Serialize>(v: &T) -> String {
    let val = serde_json::to_value(v).expect("serialize");
    canonical_value(&val)
}

fn canonical_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).unwrap(),
        serde_json::Value::Array(a) => {
            let mut out = String::from("[");
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_value(x));
            }
            out.push(']');
            out
        }
        serde_json::Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                out.push_str(&canonical_value(&o[*k]));
            }
            out.push('}');
            out
        }
    }
}

pub fn stable_id(prefix: &str, v: &impl Serialize) -> String {
    let digest = sha256_hex(canonical_json(v).as_bytes());
    format!("{}_{}", prefix, &digest[..20])
}

/* ------------------------------------------------------------------ *
 *  Bases, units and raw values
 * ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Basis {
    pub key_order: String,
    pub range_semantics: String,
    pub seq: String,
    pub size_unit: String,
}

impl Basis {
    pub fn standard() -> Self {
        Basis {
            key_order: "byte-lex; SSTable key range and range tombstone both half-open [lo,hi)".into(),
            range_semantics: "half-open [lo,hi) in byte-lex order".into(),
            seq: "global monotonic u64 sequence number; greater is newer".into(),
            size_unit: "bytes (1KiB=1024B)".into(),
        }
    }
}

/// A measurement carries its raw representation plus, when convertible,
/// a normalized value. Unconvertible measurements stay at origin with a
/// diagnostic and never block the rest of the material.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Qty {
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

impl Qty {
    pub fn parse(raw: &str) -> Qty {
        let t = raw.trim();
        let lower = t.to_ascii_lowercase();
        let (num, mul): (&str, u64) = if let Some(s) = lower.strip_suffix("kib") {
            (s, KB)
        } else if let Some(s) = lower.strip_suffix("mib") {
            (s, MB)
        } else if let Some(s) = lower.strip_suffix("kb") {
            (s, 1000)
        } else if let Some(s) = lower.strip_suffix("mb") {
            (s, 1_000_000)
        } else if let Some(s) = lower.strip_suffix('b') {
            (s, 1)
        } else {
            (t, 1)
        };
        match num.trim().parse::<u64>() {
            Ok(n) => Qty { raw: t.into(), bytes: Some(n.saturating_mul(mul)), diagnostics: vec![] },
            Err(e) => Qty {
                raw: t.into(),
                bytes: None,
                diagnostics: vec![format!("size `{}` not convertible to bytes: {}", t, e)],
            },
        }
    }

    pub fn of_bytes(n: u64) -> Qty {
        Qty { raw: format!("{}B", n), bytes: Some(n), diagnostics: vec![] }
    }
}

/// Domain value: the raw representation is always preserved verbatim.
/// `unit` is the declared coordinate/unit base (may be absent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawVal {
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/* ------------------------------------------------------------------ *
 *  WAL operations
 * ------------------------------------------------------------------ */

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    Put,
    Del,
    Range,
    Putu,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Put => "PUT",
            OpKind::Del => "DEL",
            OpKind::Range => "RANGE",
            OpKind::Putu => "PUTU",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalOp {
    pub line: usize,
    pub seq: u64,
    pub kind: OpKind,
    /// for PUT/DEL: [key]; for RANGE: [lo, hi]; for PUTU: [key, unit]
    pub args: Vec<String>,
    /// present for PUT/PUTU
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<RawVal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalDoc {
    pub ops: Vec<WalOp>,
    pub diagnostics: Vec<LocatedDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocatedDiagnostic {
    pub location: String,
    pub code: String,
    pub message: String,
}

impl WalDoc {
    /// Line-oriented WAL text.
    ///   `seq PUT key value`
    ///   `seq PUTU key unit value`  (value with explicit unit/coordinate base)
    ///   `seq DEL key`
    ///   `seq RANGE lo hi`
    /// Tokens are separated by tabs; values may contain spaces but not tabs.
    pub fn parse(text: &str) -> WalDoc {
        let mut ops = Vec::new();
        let mut diagnostics = Vec::new();
        for (idx, raw_line) in text.lines().enumerate() {
            let line = idx + 1;
            let l = raw_line.trim();
            if l.is_empty() {
                continue;
            }
            let cols: Vec<&str> = raw_line.split('\t').collect();
            if cols.len() < 2 {
                diagnostics.push(bad(line, "WAL_WHITESPACE",
                    "expected tab-separated columns: seq<TAB>KIND<TAB>..."));
                continue;
            }
            let seq = match cols[0].trim().parse::<u64>() {
                Ok(s) => s,
                Err(e) => {
                    diagnostics.push(bad(line, "WAL_SEQ", format!("bad sequence number: {}", e)));
                    continue;
                }
            };
            let kind_tok = cols[1].trim().to_ascii_uppercase();
            let parse_op = |need: usize| -> Result<(), String> { Ok(()) };
            let _ = parse_op;
            match kind_tok.as_str() {
                "PUT" => {
                    if cols.len() < 4 {
                        diagnostics.push(bad(line, "WAL_ARITY", "PUT needs: seq PUT key value"));
                        continue;
                    }
                    ops.push(WalOp { line, seq, kind: OpKind::Put, args: vec![cols[2].to_string()],
                        value: Some(RawVal { raw: cols[3..].join("\t"), unit: None }) });
                }
                "PUTU" => {
                    if cols.len() < 5 {
                        diagnostics.push(bad(line, "WAL_ARITY", "PUTU needs: seq PUTU key unit value"));
                        continue;
                    }
                    ops.push(WalOp { line, seq, kind: OpKind::Putu,
                        args: vec![cols[2].to_string(), cols[3].to_string()],
                        value: Some(RawVal { raw: cols[4..].join("\t"), unit: Some(cols[3].to_string()) }) });
                }
                "DEL" => {
                    if cols.len() != 3 {
                        diagnostics.push(bad(line, "WAL_ARITY", "DEL needs: seq DEL key"));
                        continue;
                    }
                    ops.push(WalOp { line, seq, kind: OpKind::Del, args: vec![cols[2].to_string()], value: None });
                }
                "RANGE" => {
                    if cols.len() != 4 {
                        diagnostics.push(bad(line, "WAL_ARITY", "RANGE needs: seq RANGE lo hi"));
                        continue;
                    }
                    let lo = cols[2].to_string();
                    let hi = cols[3].to_string();
                    if lo.as_str() >= hi.as_str() {
                        diagnostics.push(bad(line, "WAL_RANGE_ENDPOINT",
                            format!("range endpoint invalid: [{}, {}) must satisfy lo < hi in byte-lex order", lo, hi)));
                        continue;
                    }
                    ops.push(WalOp { line, seq, kind: OpKind::Range, args: vec![lo, hi], value: None });
                }
                other => {
                    diagnostics.push(bad(line, "WAL_KIND", format!("unknown op kind `{}`", other)));
                }
            }
        }
        WalDoc { ops, diagnostics }
    }
}

fn bad(line: usize, code: &str, message: impl Into<String>) -> LocatedDiagnostic {
    LocatedDiagnostic { location: format!("wal:{}", line), code: code.into(), message: message.into() }
}

/* ------------------------------------------------------------------ *
 *  Internal records and SSTable summaries
 * ------------------------------------------------------------------ */

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecKind {
    Put,
    Del,
    Range,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub material_id: String,
    pub source: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PointRec {
    pub key: String,
    pub seq: u64,
    pub kind: RecKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<RawVal>,
    pub origin: Origin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeRec {
    pub lo: String,
    pub hi: String,
    pub seq: u64,
    pub origin: Origin,
}

impl PointRec {
    pub fn is_del(&self) -> bool {
        matches!(self.kind, RecKind::Del)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SstFile {
    pub id: String,
    pub level: usize,
    pub lo: String,
    pub hi: String,
    pub points: Vec<PointRec>,
    pub ranges: Vec<RangeRec>,
    pub estimated_size: Qty,
    pub origin: Origin,
    pub installed_at: u64,
}

impl SstFile {
    pub fn covers_key(&self, key: &str) -> bool {
        key >= self.lo.as_str() && key < self.hi.as_str()
    }
    pub fn overlaps(&self, lo: &str, hi: &str) -> bool {
        !(self.hi.as_str() <= lo || self.lo.as_str() >= hi)
    }
    pub fn measured_bytes(&self) -> u64 {
        self.estimated_size.bytes.unwrap_or_else(|| {
            let mut n = 0u64;
            for p in &self.points {
                n += p.key.len() as u64 + 8;
                if let Some(v) = &p.value {
                    n += v.raw.len() as u64;
                }
            }
            for r in &self.ranges {
                n += r.lo.len() as u64 + r.hi.len() as u64 + 8;
            }
            n
        })
    }
}

/// Inclusive file key range as [lo, hi]; range tombstones inside are
/// half-open [lo, hi).

/// SSTable summary manifest (tab separated, may carry several files):
///   `FILE<TAB>file_id<TAB>level<TAB>lo<TAB>hi<TAB>size`
///   `PUT<TAB>seq<TAB>key<TAB>value`
///   `PUTU<TAB>seq<TAB>key<TAB>unit<TAB>value`
///   `DEL<TAB>seq<TAB>key`
///   `RANGE<TAB>seq<TAB>lo<TAB>hi`
/// Records belong to the most recently declared FILE.
pub fn parse_sst_manifest(text: &str, material_id: &str, source_name: &str)
    -> Result<Vec<SstFile>, (Vec<SstFile>, Vec<LocatedDiagnostic>)>
{
    let mut files: Vec<SstFile> = Vec::new();
    let mut diags: Vec<LocatedDiagnostic> = Vec::new();
    let mut cur: Option<usize> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line = idx + 1;
        let l = raw_line.trim();
        if l.is_empty() { continue; }
        let cols: Vec<&str> = raw_line.split('\t').collect();
        let loc = format!("{}:{}", source_name, line);
        let kind = cols[0].trim().to_ascii_uppercase();
        match kind.as_str() {
            "FILE" => {
                if cols.len() < 6 {
                    diags.push(LocatedDiagnostic { location: loc, code: "SST_FILE_ARITY".into(),
                        message: "FILE needs: FILE id level lo hi size".into() });
                    continue;
                }
                let level = match cols[2].trim().parse::<usize>() {
                    Ok(v) => v,
                    Err(e) => {
                        diags.push(LocatedDiagnostic { location: loc, code: "SST_LEVEL".into(),
                            message: format!("bad level: {}", e) });
                        continue;
                    }
                };
                let (lo, hi) = (cols[3].to_string(), cols[4].to_string());
                if lo.as_str() > hi.as_str() {
                    diags.push(LocatedDiagnostic { location: loc, code: "SST_RANGE".into(),
                        message: format!("file range invalid: lo `{}` > hi `{}`", lo, hi) });
                    continue;
                }
                let size = Qty::parse(cols[5]);
                diags.extend(size.diagnostics.iter().map(|d| LocatedDiagnostic {
                    location: loc.clone(), code: "SST_SIZE".into(), message: d.clone() }));
                let id = cols[1].trim().to_string();
                files.push(SstFile {
                    id, level, lo, hi, points: vec![], ranges: vec![],
                    estimated_size: size,
                    origin: Origin { material_id: material_id.into(), source: source_name.into(), line },
                    installed_at: 0,
                });
                cur = Some(files.len() - 1);
            }
            "PUT" | "PUTU" | "DEL" | "RANGE" => {
                let Some(idx_f) = cur else {
                    diags.push(LocatedDiagnostic { location: loc, code: "SST_NO_FILE".into(),
                        message: "record before any FILE header".into() });
                    continue;
                };
                let seq = match cols.get(1).and_then(|s| s.trim().parse::<u64>().ok()) {
                    Some(v) => v,
                    None => {
                        diags.push(LocatedDiagnostic { location: loc, code: "SST_SEQ".into(),
                            message: "bad or missing sequence number".into() });
                        continue;
                    }
                };
                let origin = Origin { material_id: material_id.into(), source: source_name.into(), line };
                match kind.as_str() {
                    "PUT" => {
                        if cols.len() < 4 {
                            diags.push(LocatedDiagnostic { location: loc, code: "SST_PUT_ARITY".into(),
                                message: "PUT needs: PUT seq key value".into() });
                            continue;
                        }
                        files[idx_f].points.push(PointRec {
                            key: cols[2].into(), seq, kind: RecKind::Put,
                            value: Some(RawVal { raw: cols[3..].join("\t"), unit: None }),
                            origin });
                    }
                    "PUTU" => {
                        if cols.len() < 5 {
                            diags.push(LocatedDiagnostic { location: loc, code: "SST_PUTU_ARITY".into(),
                                message: "PUTU needs: PUTU seq key unit value".into() });
                            continue;
                        }
                        files[idx_f].points.push(PointRec {
                            key: cols[2].into(), seq, kind: RecKind::Put,
                            value: Some(RawVal { raw: cols[4..].join("\t"), unit: Some(cols[3].into()) },
                            origin });
                    }
                    "DEL" => {
                        if cols.len() != 3 {
                            diags.push(LocatedDiagnostic { location: loc, code: "SST_DEL_ARITY".into(),
                                message: "DEL needs: DEL seq key".into() });
                            continue;
                        }
                        files[idx_f].points.push(PointRec {
                            key: cols[2].into(), seq, kind: RecKind::Del, value: None, origin });
                    }
                    "RANGE" => {
                        if cols.len() != 4 {
                            diags.push(LocatedDiagnostic { location: loc, code: "SST_RANGE_ARITY".into(),
                                message: "RANGE needs: RANGE seq lo hi".into() });
                            continue;
                        }
                        let (lo, hi) = (cols[2].to_string(), cols[3].to_string());
                        if lo.as_str() >= hi.as_str() {
                            diags.push(LocatedDiagnostic { location: loc, code: "SST_RANGE_ENDPOINT".into(),
                                message: format!("range endpoint invalid: [{}, {}) lo < hi required", lo, hi) });
                            continue;
                        }
                        files[idx_f].ranges.push(RangeRec { lo, hi, seq, origin });
                    }
                    _ => unreachable!(),
                }
            }
            other => {
                diags.push(LocatedDiagnostic { location: loc, code: "SST_KIND".into(),
                    message: format!("unknown record kind `{}`", other) });
            }
        }
    }

    if diags.is_empty() { Ok(files) } else { Err((files, diags)) }
}

/* ------------------------------------------------------------------ *
 *  Live state
 * ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub seq: u64,
    pub label: String,
    pub created_at: u64,
    pub released: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveState {
    pub basis: Basis,
    pub files: Vec<SstFile>,
    pub snapshots: Vec<Snapshot>,
    pub rule_version: String,
    pub rule_revisions: Vec<RuleRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuleRevision {
    pub version: String,
    pub changed_at: u64,
    pub reason: String,
    pub tie_policy: TiePolicy,
    pub parent_version: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TiePolicy {
    /// Internal-key type order: PUT outranks DEL at the same sequence.
    TypeOrder,
    /// Deletion outranks PUT at the same sequence.
    DelWins,
    /// Same-sequence multi-type collision is a semantic dead-end.
    Reject,
}

impl LiveState {
    pub fn empty() -> LiveState {
        LiveState {
            basis: Basis::standard(),
            files: vec![],
            snapshots: vec![],
            rule_version: "rules-v1".into(),
            rule_revisions: vec![RuleRevision {
                version: "rules-v1".into(),
                changed_at: 0,
                reason: "baseline: internal key order (seq desc, PUT>DEL at equal seq)".into(),
                tie_policy: TiePolicy::TypeOrder,
                parent_version: None,
            }],
        }
    }

    pub fn active_snapshots(&self) -> impl Iterator<Item = &Snapshot> {
        self.snapshots.iter().filter(|s| !s.released)
    }

    /// Smallest active snapshot sequence; tombstones newer than this may
    /// still cover a version an old snapshot must read.
    pub fn oldest_active_seq(&self) -> Option<u64> {
        self.active_snapshots().map(|s| s.seq).min()
    }

    pub fn files_at(&self, level: usize) -> Vec<&SstFile> {
        let mut out: Vec<&SstFile> = self.files.iter().filter(|f| f.level == level).collect();
        out.sort_by(|a, b| a.lo.cmp(&b.lo).then(a.hi.cmp(&b.hi)).then(a.id.cmp(&b.id)));
        out
    }

    pub fn max_level(&self) -> usize {
        self.files.iter().map(|f| f.level).max().unwrap_or(0)
    }
}

/* ------------------------------------------------------------------ *
 *  Read path: internal-key order and snapshot visibility
 * ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HideReason {
    NewerValue,
    NewerPointDelete,
    RangeTombstone { range_seq: u64, lo: String, hi: String },
    SupersededSameSeq { winner: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainEntry {
    pub key: String,
    pub seq: u64,
    pub kind: RecKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<RawVal>,
    pub sst_id: String,
    pub level: usize,
    pub origin: Origin,
    pub visible: bool,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_by: Option<HideReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadResult {
    pub snapshot: String,
    pub snapshot_seq: u64,
    pub key: String,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<RawVal>,
    pub winner_sst: Option<String>,
    pub chain: Vec<ChainEntry>,
}

fn type_rank(k: RecKind) -> u8 {
    match k {
        RecKind::Put => 2,
        RecKind::Del => 1,
        RecKind::Range => 0,
    }
}

/// Ordering used by the merge iterator: newest seq first; within one
/// sequence the internal-key type order applies (PUT > DEL).
pub fn point_order(a: &PointRec, b: &PointRec) -> std::cmp::Ordering {
    b.seq.cmp(&a.seq)
        .then_with(|| type_rank(b.kind).cmp(&type_rank(a.kind)))
        .then_with(|| a.key.cmp(&b.key))
}

fn covering_ranges<'a>(state: &'a LiveState, key: &str, at_or_before: u64)
    -> Vec<&'a RangeRec>
{
    let mut out = Vec::new();
    for f in &state.files {
        for r in &f.ranges {
            if r.seq <= at_or_before && key >= r.lo.as_str() && key < r.hi.as_str() {
                out.push(r);
            }
        }
    }
    out
}

/// Read one key with snapshot visibility. The chain records every
/// surviving internal version and why older ones are hidden.
pub fn read_key(state: &LiveState, key: &str, snapshot_seq: Option<u64>) -> ReadResult {
    let snap_label = match snapshot_seq {
        Some(s) => format!("snap@{}", s),
        None => "head".into(),
    };
    let horizon = snapshot_seq.unwrap_or(u64::MAX);

    let mut points: Vec<(&PointRec, &SstFile)> = state.files.iter()
        .filter(|f| f.covers_key(key))
        .flat_map(|f| f.points.iter().filter(|p| p.key == key && p.seq <= horizon).map(move |p| (p, f)))
        .collect();
    points.sort_by(|(a, _), (b, _)| point_order(a, b));

    let ranges = covering_ranges(state, key, horizon);
    let newest_range = ranges.iter().map(|r| r.seq).max();

    let mut chain = Vec::new();
    let mut winner: Option<&PointRec> = None;
    let mut winner_file: Option<&SstFile> = None;
    let mut stop = false;

    for (p, f) in &points {
        if stop { break; }
        let mut visible = false;
        let mut reason;
        let mut hidden_by = None;

        if let Some(rs) = newest_range {
            if p.seq < rs {
                reason = format!("covered by range tombstone seq {} ([half-open, lo<=key<hi])", rs);
                let r = ranges.iter().find(|r| r.seq == rs).unwrap();
                hidden_by = Some(HideReason::RangeTombstone { range_seq: rs, lo: r.lo.clone(), hi: r.hi.clone() });
            } else if p.seq == rs {
                // same seq: range delete vs point — type order tie policy
                match state.rule_revisions.last().map(|r| r.tie_policy).unwrap_or(TiePolicy::TypeOrder) {
                    TiePolicy::TypeOrder => {
                        winner = Some(p); winner_file = Some(f); visible = true;
                        reason = "same seq as range delete: internal-key type order keeps PUT/DEL".into();
                        stop = true;
                    }
                    TiePolicy::DelWins => {
                        reason = "same seq as range delete: DEL_WINS revision hides the point".into();
                        let r = ranges.iter().find(|r| r.seq == rs).unwrap();
                        hidden_by = Some(HideReason::RangeTombstone { range_seq: rs, lo: r.lo.clone(), hi: r.hi.clone() });
                    }
                    TiePolicy::Reject => {
                        reason = "same seq as range delete: REJECT policy — semantic ambiguity, no winner".into();
                        hidden_by = Some(HideReason::SupersededSameSeq { winner: "UNRESOLVED".into() });
                    }
                }
            } else {
                winner = Some(p); winner_file = Some(f); visible = true;
                reason = "newest internal version for this key".into();
                stop = true;
            }
        } else if winner.is_none() {
            winner = Some(p); winner_file = Some(f); visible = true;
            reason = if p.is_del() { "newest version is a point tombstone".into() }
                     else { "newest internal version for this key".into() };
            stop = true;
        } else {
            reason = format!("hidden by newer point version seq {}", winner.unwrap().seq);
            hidden_by = Some(if winner.unwrap().is_del() {
                HideReason::NewerPointDelete
            } else {
                HideReason::NewerValue
            });
        }

        chain.push(ChainEntry {
            key: p.key.clone(), seq: p.seq, kind: p.kind, value: p.value.clone(),
            sst_id: f.id.clone(), level: f.level, origin: p.origin.clone(),
            visible, reason, hidden_by,
        });
    }

    let (found, value) = match winner {
        Some(w) if !w.is_del() && visible_wins(state, w.seq, newest_range) => (true, w.value.clone()),
        _ => (false, None),
    };

    ReadResult {
        snapshot: snap_label,
        snapshot_seq: snapshot_seq.unwrap_or(horizon),
        key: key.into(),
        found,
        value,
        winner_sst: winner_file.map(|f| f.id.clone()),
        chain,
    }
}

fn visible_wins(state: &LiveState, seq: u64, newest_range: Option<u64>) -> bool {
    if newest_range.is_some_and(|rs| {
        match state.rule_revisions.last().map(|r| r.tie_policy).unwrap_or(TiePolicy::TypeOrder) {
            TiePolicy::TypeOrder => seq < rs,
            TiePolicy::DelWins => seq <= rs,
            TiePolicy::Reject => seq <= rs,
        }
    }) {
        return false;
    }
    true
}

/// Full key version chain including records hidden even from head
/// (used by the "key 版本链" view and by snapshot comparisons).
pub fn version_chain(state: &LiveState, key: &str) -> Vec<ChainEntry> {
    let mut chains = Vec::new();
    let head = read_key(state, key, None);
    chains.extend(head.chain);
    // ensure versions only visible to old snapshots also appear
    let mut known: std::collections::HashSet<(u64, RecKind, String)> =
        chains.iter().map(|c| (c.seq, c.kind, c.sst_id.clone())).collect();
    for f in &state.files {
        for p in &f.points {
            if p.key == key && known.insert((p.seq, p.kind, f.id.clone())) {
                chains.push(ChainEntry {
                    key: p.key.clone(), seq: p.seq, kind: p.kind, value: p.value.clone(),
                    sst_id: f.id.clone(), level: f.level, origin: p.origin.clone(),
                    visible: false, reason: "beyond head horizon / duplicate internal key".into(),
                    hidden_by: None,
                });
            }
        }
    }
    chains.sort_by(|a, b| b.seq.cmp(&a.seq).then(type_rank(b.kind).cmp(&type_rank(a.kind))));
    chains
}
