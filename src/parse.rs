//! 导入/解析层。
//!
//! 分层原则：不可变材料保存*原始表示*（raw_text，含原始单位/坐标写法），
//! 只有派生层才规范化。无法转换的值不丢弃：以 Unconverted 材料保留在原位，
//! 并产生带位置的诊断（code 以 E_ 开头）。

use crate::domain::{PointEntry, RangeTombstone, VType};
use crate::error::Diagnostic;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialKind {
    Wal,
    Flush,
    SstSummary,
    SnapshotPin,
    CompactionSummary,
    Unconverted,
}

impl MaterialKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MaterialKind::Wal => "wal",
            MaterialKind::Flush => "flush",
            MaterialKind::SstSummary => "sst_summary",
            MaterialKind::SnapshotPin => "snapshot_pin",
            MaterialKind::CompactionSummary => "compaction_summary",
            MaterialKind::Unconverted => "unconverted",
        }
    }
    pub fn parse(s: &str) -> MaterialKind {
        match s {
            "wal" => MaterialKind::Wal,
            "flush" => MaterialKind::Flush,
            "sst_summary" => MaterialKind::SstSummary,
            "snapshot_pin" => MaterialKind::SnapshotPin,
            "compaction_summary" => MaterialKind::CompactionSummary,
            _ => MaterialKind::Unconverted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Material {
    pub id: String,
    pub batch_id: String,
    pub ordinal: usize,
    pub kind: MaterialKind,
    /// 原始表示（逐字节保留；导入往返身份依赖它）。
    pub raw_text: String,
    /// 原始单位/坐标基准标注（不做单位换算时仍保留）。
    pub declared_basis: Option<String>,
    /// 解析出的规范化点记录（Unconverted 时为空）。
    pub point: Option<PointEntry>,
    pub range: Option<RangeTombstone>,
    /// flush 摘要。
    pub flush: Option<FlushSpec>,
    pub pin: Option<PinSpec>,
    /// 无法转换/不完整诊断（材料保留，不伪装成功）。
    pub diagnostics: Vec<Diagnostic>,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushSpec {
    pub flush_id: String,
    pub point_material_ids: Vec<String>,
    pub range_material_ids: Vec<String>,
    pub level: u32,
    pub size_bytes: Option<u64>,
    pub size_raw: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinSpec {
    pub snapshot_id: String,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedBatch {
    pub batch_id: String,
    pub materials: Vec<Material>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
struct WalJson {
    op: String,
    key: String,
    #[serde(default)]
    end_exclusive: Option<String>,
    seq: serde_json::Value,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    basis: Option<String>,
}

fn parse_seq(v: &serde_json::Value, loc: &str, diags: &mut Vec<Diagnostic>) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| {
            diags.push(Diagnostic::new("E_SEQ_NON_U64", loc, "sequence number 必须是非负整数".into()));
            None
        }),
        serde_json::Value::String(s) => match s.trim().parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                diags.push(Diagnostic::new("E_SEQ_PARSE", loc, format!("无法把 {s:?} 转为 uint64 seq")));
                None
            }
        },
        other => {
            diags.push(Diagnostic::new("E_SEQ_MISSING", loc, format!("seq 字段类型非法: {other}")));
            None
        }
    }
}

/// 解析一行 WAL 文本。规范形式：`PUT key seq [value]` / `DELETE key seq` /
/// `SINGLE_DELETE key seq` / `RANGE start end seq`（大小写不敏感，# 后为注释）。
fn parse_wal_text(text: &str, loc: &str, diags: &mut Vec<Diagnostic>) -> Option<(Option<PointEntry>, Option<RangeTombstone>)> {
    let body = text.split('#').next().unwrap_or("").trim();
    if body.is_empty() {
        return None;
    }
    let parts: Vec<&str> = body.split_whitespace().collect();
    let vt = VType::parse(parts[0]);
    let is_range = parts[0].eq_ignore_ascii_case("range")
        || parts[0].eq_ignore_ascii_case("range_delete")
        || parts[0].eq_ignore_ascii_case("rangetombstone");
    if !is_range && vt.is_none() {
        diags.push(Diagnostic::new("E_OP_UNKNOWN", loc, format!("未知操作 {:?}", parts[0])));
        return Some((None, None));
    }
    if is_range {
        if parts.len() < 4 {
            diags.push(Diagnostic::new("E_RANGE_INCOMPLETE", loc, "range 需要 START END SEQ 三个参数".into()));
            return Some((None, None));
        }
        match parts[3].parse::<u64>() {
            Ok(seq) => {
                let (s, e) = (parts[1].to_string(), parts[2].to_string());
                if s.as_str() >= e.as_str() {
                    diags.push(Diagnostic::new(
                        "E_RANGE_ENDPOINT",
                        loc,
                        "range 删除端点必须满足 start < end（半开 [start,end)）",
                    ));
                }
                Some((None, Some(RangeTombstone {
                    start_inclusive: s,
                    end_exclusive: e,
                    seq,
                    origin_material: String::new(),
                    origin_sst: None,
                })))
            }
            Err(_) => {
                diags.push(Diagnostic::new("E_SEQ_PARSE", loc, format!("seq {:?} 非 uint64", parts[3])));
                Some((None, None))
            }
        }
    } else {
        let vt = vt.unwrap();
        if parts.len() < 3 {
            diags.push(Diagnostic::new("E_WAL_INCOMPLETE", loc, "point 记录至少需要 OP KEY SEQ".into()));
            return Some((None, None));
        }
        match parts[2].parse::<u64>() {
            Ok(seq) => {
                let value = if parts.len() >= 4 { Some(parts[3..].join(" ")) } else { None };
                if vt != VType::Put && vt != VType::Merge && value.is_some() {
                    diags.push(Diagnostic::new(
                        "E_TOMBSTONE_VALUE",
                        loc,
                        "tombstone 不应携带 value（保留原值但记录诊断）",
                    ));
                }
                Some((Some(PointEntry {
                    user_key: parts[1].to_string(),
                    seq,
                    vtype: vt,
                    value,
                    origin_material: String::new(),
                    origin_sst: None,
                }), None))
            }
            Err(_) => {
                diags.push(Diagnostic::new("E_SEQ_PARSE", loc, format!("seq {:?} 非 uint64", parts[2])));
                Some((None, None))
            }
        }
    }
}

/// 解析一个导入批次。每条材料独立处理，失败材料以 Unconverted 保留。
pub fn parse_batch(batch_id: &str, lines: &[(String, Option<String>)]) -> ParsedBatch {
    let mut materials = Vec::new();
    let mut diagnostics = Vec::new();
    for (ordinal, (raw, kind_hint)) in lines.iter().enumerate() {
        let loc = format!("batch {batch_id} / line {}", ordinal + 1);
        let mut mdiags: Vec<Diagnostic> = Vec::new();
        let mut kind = kind_hint.as_deref().map(MaterialKind::parse).unwrap_or(MaterialKind::Wal);
        let mut point = None;
        let mut range = None;
        let mut flush = None;
        let mut pin = None;
        let mut basis = None;

        let trimmed = raw.trim();
        let parsed_json = serde_json::from_str::<serde_json::Value>(trimmed).ok().and_then(|v| {
            if v.is_object() { Some(v) } else { None }
        });

        if let Some(obj) = parsed_json {
            let rec_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("wal");
            match rec_type {
                "flush" => {
                    kind = MaterialKind::Flush;
                    match serde_json::from_value::<serde_json::Value>(obj.clone()) {
                        Ok(f) => {
                            basis = f.get("basis").and_then(|b| b.as_str()).map(String::from);
                            let level = f.get("level").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
                            let size_raw = f.get("size").and_then(|s| match s {
                                serde_json::Value::String(s) => Some(s.clone()),
                                serde_json::Value::Number(n) => Some(n.to_string()),
                                _ => None,
                            });
                            let size_bytes = match &size_raw {
                                Some(s) => match s.trim_end_matches(|c: char| c.is_alphabetic() || c == ' ').trim().parse::<u64>() {
                                    Ok(n) => Some(n),
                                    Err(_) => {
                                        mdiags.push(Diagnostic::new("E_SIZE_UNIT", &loc,
                                            format!("size {s:?} 无法转换为 bytes，保留原始表示")));
                                        None
                                    }
                                },
                                None => None,
                            };
                            let mut pids = Vec::new();
                            if let Some(arr) = f.get("points").and_then(|v| v.as_array()) {
                                for (i, v) in arr.iter().enumerate() {
                                    if let Some(s) = v.as_str() { pids.push(s.to_string()); }
                                    else { mdiags.push(Diagnostic::new("E_FLUSH_REF", format!("{loc}/points[{i}]"), "材料引用必须是字符串 id".into())); }
                                }
                            }
                            let mut rids = Vec::new();
                            if let Some(arr) = f.get("ranges").and_then(|v| v.as_array()) {
                                for (i, v) in arr.iter().enumerate() {
                                    if let Some(s) = v.as_str() { rids.push(s.to_string()); }
                                    else { mdiags.push(Diagnostic::new("E_FLUSH_REF", format!("{loc}/ranges[{i}]"), "材料引用必须是字符串 id".into())); }
                                }
                            }
                            flush = Some(FlushSpec {
                                flush_id: f.get("flush_id").or_else(|| f.get("id")).and_then(|i| i.as_str()).unwrap_or("flush").to_string(),
                                point_material_ids: pids,
                                range_material_ids: rids,
                                level,
                                size_bytes,
                                size_raw,
                            });
                        }
                        Err(e) => mdiags.push(Diagnostic::new("E_FLUSH_SHAPE", &loc, e.to_string())),
                    }
                }
                "snapshot_pin" => {
                    kind = MaterialKind::SnapshotPin;
                    let sid = obj.get("snapshot_id").or_else(|| obj.get("id")).and_then(|i| i.as_str());
                    let seq = obj.get("seq").map(|s| parse_seq(s, &loc, &mut mdiags)).flatten();
                    match (sid, seq) {
                        (Some(sid), Some(seq)) => pin = Some(PinSpec { snapshot_id: sid.to_string(), seq }),
                        _ => mdiags.push(Diagnostic::new("E_PIN_INCOMPLETE", &loc, "snapshot_pin 需要 snapshot_id 与 seq".into())),
                    }
                }
                "compaction_summary" => {
                    kind = MaterialKind::CompactionSummary;
                    // 历史压实摘要作为不可变材料保存（叙述证据），不参与重放安装。
                }
                _ => {
                    kind = MaterialKind::Wal;
                    match serde_json::from_value::<WalJson>(obj.clone()) {
                        Ok(w) => {
                            basis = w.basis.clone();
                            let seq = parse_seq(&w.seq, &loc, &mut mdiags);
                            if let Some(seq) = seq {
                                let opc = w.op.to_ascii_lowercase();
                                if opc == "range" || opc == "range_delete" {
                                    match w.end_exclusive {
                                        Some(end) => {
                                            if w.key.as_str() >= end.as_str() {
                                                mdiags.push(Diagnostic::new("E_RANGE_ENDPOINT", &loc,
                                                    "range 删除端点必须满足 start < end（半开 [start,end)）"));
                                            }
                                            range = Some(RangeTombstone { start_inclusive: w.key, end_exclusive: end, seq, origin_material: String::new(), origin_sst: None });
                                        }
                                        None => mdiags.push(Diagnostic::new("E_RANGE_INCOMPLETE", &loc, "range 删除缺少 end_exclusive".into())),
                                    }
                                } else if let Some(vt) = VType::parse(&w.op) {
                                    point = Some(PointEntry {
                                        user_key: w.key, seq, vtype: vt, value: w.value,
                                        origin_material: String::new(), origin_sst: None,
                                    });
                                } else {
                                    mdiags.push(Diagnostic::new("E_OP_UNKNOWN", &loc, format!("未知操作 {:?}", w.op)));
                                }
                            }
                        }
                        Err(e) => mdiags.push(Diagnostic::new("E_WAL_SHAPE", &loc, e.to_string())),
                    }
                }
            }
        } else {
            if let Some((p, r)) = parse_wal_text(raw, &loc, &mut mdiags) {
                point = p;
                range = r;
            }
        }

        let usable = matches!(kind, MaterialKind::Wal | MaterialKind::Flush | MaterialKind::SnapshotPin | MaterialKind::CompactionSummary)
            && (point.is_some() || range.is_some() || flush.is_some() || pin.is_some() || matches!(kind, MaterialKind::CompactionSummary));
        let effective_kind = if !mdiags.is_empty() && !usable { MaterialKind::Unconverted } else { kind };

        let id = crate::ids::material_id(batch_id, ordinal, raw);
        if let Some(p) = point.as_mut() { p.origin_material = id.clone(); }
        if let Some(r) = range.as_mut() { r.origin_material = id.clone(); }
        let m = Material {
            id,
            batch_id: batch_id.to_string(),
            ordinal,
            kind: effective_kind,
            raw_text: raw.clone(),
            declared_basis: basis,
            point,
            range,
            flush,
            pin,
            diagnostics: mdiags.clone(),
            content_hash: crate::ids::content_hash(raw),
        };
        materials.push(m);
        diagnostics.extend(mdiags);
    }
    ParsedBatch { batch_id: batch_id.to_string(), materials, diagnostics }
}
