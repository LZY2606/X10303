//! 导入层：原始材料 -> 可重放的导入批次。
//! 规则：结构校验不过即返回带位置诊断；无法转换的值留在原位置；等价输入得到稳定顺序。

use crate::domain::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlushOrder {
    pub id: String,
    pub seq_no: i64,
    pub wal_ids: Vec<String>,
    pub output_file_id: String,
    pub level: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ImportBatch {
    pub tables: Vec<SSTable>,
    pub snapshots: Vec<Snapshot>,
    pub wal: Vec<WalRecord>,
    pub flushes: Vec<FlushOrder>,
    pub diagnostics: Vec<Diagnostic>,
}

/// 校验并解码一批不可变材料；错误都是带位置的诊断。
pub fn decode_materials(materials: &[RawMaterial]) -> Result<ImportBatch, Vec<Diagnostic>> {
    let mut diags: Vec<Diagnostic>> = Vec::new();
    let mut batch = ImportBatch::default();
    for mat in materials {
        let loc = format!("material[{}:{}]", mat.kind_id(), mat.id);
        match mat.kind {
            MaterialKind::WalRecord => match serde_json::from_value::<WalRecord>(mat.payload.clone()) {
                Ok(w) => {
                    if w.seq.is_none() {
                        diags.push(err(&loc, "missing_field", "WAL 记录缺少 seq（序列号必须显式给出）"));
                    }
                    if w.kind == WalKind::Set && w.value.is_none() {
                        diags.push(err(&loc, "missing_field", "Set 记录缺少 value；不得用空值伪装成功"));
                    }
                    if w.kind == WalKind::RangeDelete && w.range_end.is_none() {
                        diags.push(err(&loc, "missing_field", "range_delete 缺少 range_end"));
                    }
                    if let Ok(k) = w.key.bytes() { let _ = k; } else if let Err(d) = w.key.bytes() {
                        diags.push(prefix(d, &loc));
                    }
                    batch.wal.push(w);
                }
                Err(e) => diags.push(err(&loc, "corrupt_wal", &format!("WAL 载荷损坏: {e}"))),
            },
            MaterialKind::FlushOrder => match serde_json::from_value::<FlushOrder>(mat.payload.clone()) {
                Ok(fo) => batch.flushes.push(fo),
                Err(e) => diags.push(err(&loc, "corrupt_flush_order", &format!("flush 指令损坏: {e}"))),
            },
            MaterialKind::SSTableSummary => match serde_json::from_value::<SSTable>(mat.payload.clone()) {
                Ok(t) => {
                    if let Err(d) = validate_table(&t) { diags.push(prefix(d, &loc)); }
                    batch.tables.push(t);
                }
                Err(e) => diags.push(err(&loc, "corrupt_sstable", &format!("SSTable 摘要损坏: {e}"))),
            },
            MaterialKind::Snapshot => match serde_json::from_value::<Snapshot>(mat.payload.clone()) {
                Ok(s) => batch.snapshots.push(s),
                Err(e) => diags.push(err(&loc, "corrupt_snapshot", &format!("snapshot 损坏: {e}"))),
            },
        }
    }
    // 稳定顺序：等价输入必须得到等价结果。
    batch.wal.sort_by(|a, b| a.lsn.cmp(&b.lsn).then_with(|| a.id.cmp(&b.id)));
    batch.flushes.sort_by_key(|f| f.seq_no);
    batch.tables.sort_by(|a, b| a.level.cmp(&b.level).then_with(|| a.id.cmp(&b.id)));
    batch.snapshots.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)));

    // flush 指令引用完整性。
    let wal_ids: std::collections::BTreeSet<&str> = batch.wal.iter().map(|w| w.id.as_str()).collect();
    for fo in &batch.flushes {
        for wid in &fo.wal_ids {
            if !wal_ids.contains(wid.as_str()) {
                diags.push(err(
                    &format!("material[flush_order:{}]", fo.id),
                    "dangling_reference",
                    &format!("flush 引用了不存在的 WAL 记录 {}", wid),
                ));
            }
        }
    }
    // 身份唯一性。
    let mut seen = std::collections::BTreeSet::new();
    for t in &batch.tables {
        if !seen.insert(t.id.clone()) {
            diags.push(err(&format!("material[sstable:{}]", t.id), "duplicate_id", "SSTable id 重复"));
        }
    }
    if diags.iter().any(|d| d.severity == DiagSeverity::Error) {
        return Err(diags);
    }
    batch.diagnostics = diags;
    Ok(batch)
}

fn validate_table(t: &SSTable) -> Result<(), Diagnostic> {
    let r = t.key_range.decoded()?;
    if !r.is_valid_half_open() {
        return Err(Diagnostic {
            code: "invalid_half_open_range".into(), severity: DiagSeverity::Error,
            location: format!("sstable[{}].key_range", t.id),
            message: "半开区间非法（start>=end）".into(), candidates: vec![],
        });
    }
    if t.size.magnitude.is_none() {
        return Err(t.size.magnitude_or().unwrap_err());
    }
    for p in &t.points {
        p.key.bytes()?;
        if p.kind == PointKind::Set && p.value.is_none() {
            return Err(Diagnostic {
                code: "missing_point_value".into(), severity: DiagSeverity::Error,
                location: format!("sstable[{}].point[{}]", t.id, p.id),
                message: "Set 记录缺 value".into(), candidates: vec![],
            });
        }
    }
    for rg in &t.ranges { rg.range.decoded()?; }
    Ok(())
}

fn err(loc: &str, code: &str, msg: &str) -> Diagnostic {
    Diagnostic { code: code.into(), severity: DiagSeverity::Error, location: loc.into(), message: msg.into(), candidates: vec![] }
}
fn prefix(mut d: Diagnostic, loc: &str) -> Diagnostic {
    d.location = format!("{loc}.{}", d.location);
    d
}

impl MaterialKind {
    pub fn kind_id(&self) -> &'static str {
        match self {
            MaterialKind::WalRecord => "wal_record",
            MaterialKind::FlushOrder => "flush_order",
            MaterialKind::SSTableSummary => "sstable_summary",
            MaterialKind::Snapshot => "snapshot",
        }
    }
}

/// 按 flush 指令把 WAL 回放成一个 memtable SSTable（叙事：WAL -> memtable -> flush）。
pub fn materialize_flush(fo: &FlushOrder, wal_by_id: &BTreeMap<String, WalRecord>, material_ids: &[String]) -> Result<SSTable, Diagnostic> {
    let mut points = Vec::new();
    let mut ranges = Vec::new();
    let mut min_key: Option<Endpoint> = None;
    let mut max_key: Option<Endpoint> = None;
    for wid in &fo.wal_ids {
        let w = wal_by_id.get(wid).ok_or_else(|| err(
            &format!("flush[{}]", fo.id), "dangling_reference", &format!("WAL {} 不存在", wid)))?;
        let seq = w.seq.ok_or_else(|| err(
            &format!("flush[{}].wal[{}]", fo.id, w.id), "missing_field", "WAL 缺 seq"))?;
        match w.kind {
            WalKind::Set | WalKind::Delete => {
                points.push(PointRecord {
                    id: format!("{}.p{}", fo.output_file_id, points.len() + 1),
                    seq,
                    kind: if w.kind == WalKind::Delete { PointKind::Delete } else { PointKind::Set },
                    key: w.key.clone(),
                    value: w.value.clone(),
                    diagnostics: vec![],
                });
                min_key = Some(min_key.map_or_else(|| w.key.clone(), |k| if endpoint_lt(&w.key, &k) { w.key.clone() } else { k }));
                max_key = Some(max_key.map_or_else(|| w.key.clone(), |k| if endpoint_lt(&k, &w.key) { w.key.clone() } else { k }));
            }
            WalKind::RangeDelete => {
                let end = w.range_end.clone().ok_or_else(|| err(
                    &format!("flush[{}].wal[{}]", fo.id, w.id), "missing_field", "range_delete 缺 end"))?;
                let kr = KeyRange { start: w.key.clone(), end, interval_basis: "byte-lexicographic [start,end)".into() };
                ranges.push(RangeTomb { id: format!("{}.r{}", fo.output_file_id, ranges.len() + 1), seq, range: kr, fragments: vec![] });
                min_key = Some(min_key.map_or_else(|| w.key.clone(), |k| if endpoint_lt(&w.key, &k) { w.key.clone() } else { k }));
                max_key = Some(max_key.map_or_else(|| w.range_end.clone().unwrap_or(w.key.clone()), |k| {
                    w.range_end.as_ref().map_or(k.clone(), |e| if endpoint_lt(&k, e) { e.clone() } else { k })
                }));
            }
        }
    }
    let (start, end) = match (min_key, max_key) {
        (Some(a), Some(b)) => (a, b),
        None => return Err(err(&format!("flush[{}]", fo.id), "empty_flush", "flush 的 WAL 集合为空，无 key 区间")),
    };
    let approx = points.iter().map(|p| p.value.as_ref().map(|v| v.representation.len() as i64).unwrap_or(0) + 8).sum::<i64>()
        + ranges.len() as i64 * 8 + 8;
    Ok(SSTable {
        id: fo.output_file_id.clone(),
        level: fo.level,
        state: FileState::Live,
        origin: FileOrigin::MemtableFlush,
        key_range: KeyRange { start, end, interval_basis: "byte-lexicographic [start,end)".into() },
        size: Measured::bytes(&format!("{} byte", approx), approx),
        points,
        ranges,
        provenance: Provenance::material(material_ids),
    })
}

fn endpoint_lt(a: &Endpoint, b: &Endpoint) -> bool {
    match (a.bytes(), b.bytes()) {
        (Ok(x), Ok(y)) => x < y,
        _ => a.raw < b.raw,
    }
}

/// 生成导入事件载荷辅助。
pub fn material_event_payload(mat: &RawMaterial) -> Value {
    serde_json::to_value(mat).unwrap_or(json!({"id":mat.id}))
}
