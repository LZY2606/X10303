//! WAL 导入：按字节词法键序的行式 WAL。
//! 行格式：`<seq>|<kind>|<key>|<value-or-rangeend>`
//! kind ∈ put | del | rangedel（rangedel 的第 4 列为排他终点）。
//! 损坏与不完整分别给出带位置的诊断，坏窗口不进入 flush，原值保留。

use crate::event::{Affected, Evidence, Fact};
use crate::types::{Entry, Kind, LoadFailure, Origin};

pub struct ParsedWal {
    pub entries: Vec<Entry>,
    pub failures: Vec<LoadFailure>,
}

pub fn parse_wal(material_id: &str, lines: &[String]) -> ParsedWal {
    let mut entries = Vec::new();
    let mut failures = Vec::new();
    for (i, raw_line) in lines.iter().enumerate() {
        let line_no = i + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('|').collect();
        let origin = || Origin {
            material_id: material_id.to_string(),
            offset: i,
            field: None,
            line: Some(line_no),
        };
        if cols.len() < 4 {
            failures.push(LoadFailure::Incomplete {
                origin: origin(),
                missing_field: format!("expected 4 columns, found {}", cols.len()),
                raw_snippet: raw_line.clone(),
            });
            continue;
        }
        let seq = match cols[0].trim().parse::<u64>() {
            Ok(s) => s,
            Err(_) => {
                failures.push(LoadFailure::Corrupt {
                    origin: Origin { field: Some("seq".into()), ..origin() },
                    reason: "seq is not an unsigned integer".into(),
                    raw_snippet: raw_line.clone(),
                });
                continue;
            }
        };
        let kind = match cols[1].trim() {
            "put" => Kind::Put,
            "del" => Kind::Del,
            "rangedel" => Kind::RangeDel,
            other => {
                failures.push(LoadFailure::Corrupt {
                    origin: Origin { field: Some("kind".into()), ..origin() },
                    reason: format!("unknown entry kind {:?}", other),
                    raw_snippet: raw_line.clone(),
                });
                continue;
            }
        };
        let key = cols[2].trim();
        if key.is_empty() {
            failures.push(LoadFailure::Incomplete {
                origin: Origin { field: Some("key".into()), ..origin() },
                missing_field: "user key is empty".into(),
                raw_snippet: raw_line.clone(),
            });
            continue;
        }
        let (value, range_end) = match kind {
            Kind::Put => (Some(cols[3].trim().to_string()), None),
            Kind::Del => (None, None),
            Kind::RangeDel => {
                let end = cols[3].trim();
                if end.is_empty() || end <= key {
                    failures.push(LoadFailure::Corrupt {
                        origin: Origin { field: Some("range_end".into()), ..origin() },
                        reason: "range tombstone end must be non-empty and greater than start"
                            .into(),
                        raw_snippet: raw_line.clone(),
                    });
                    continue;
                }
                (None, Some(end.to_string()))
            }
        };
        entries.push(Entry {
            entry_id: format!("{}#L{}", material_id, line_no),
            key: key.to_string(),
            seq,
            kind,
            value,
            range_end,
            origin: origin(),
        });
    }
    ParsedWal { entries, failures }
}

/// 为导入/flush 构造“坏窗口被排除”的证据。
pub fn failure_evidence(
    evidence_id: &str,
    material_id: &str,
    failures: &[LoadFailure],
) -> Option<Evidence> {
    if failures.is_empty() {
        return None;
    }
    let mut basis = Vec::new();
    for f in failures {
        let (cls, origin, field_or_reason, snippet) = match f {
            LoadFailure::Corrupt { origin, reason, raw_snippet } => {
                ("format_corrupt", origin, reason.as_str(), raw_snippet)
            }
            LoadFailure::Incomplete { origin, missing_field, raw_snippet } => {
                ("input_incomplete", origin, missing_field.as_str(), raw_snippet)
            }
        };
        basis.push(Fact {
            predicate: "import_window_excluded".into(),
            args: map! {
                "class".into() => cls.into(),
                "origin".into() => origin.to_string(),
                "reason".into() => field_or_reason.into(),
                "raw".into() => snippet.clone(),
            },
        });
    }
    Some(Evidence {
        evidence_id: evidence_id.into(),
        title: format!("损坏/不完整窗口在 {} 中被排除", material_id),
        class: "import_diagnostic".into(),
        basis,
        affected: vec![Affected {
            kind: "material".into(),
            id: material_id.into(),
            detail: Some(format!("{} 个窗口未参与派生", failures.len())),
        }],
    })
}

#[macro_export]
macro_rules! map {
    ( $( $k:expr => $v:expr ),* $(,)? ) => {{
        let mut m = std::collections::BTreeMap::new();
        $( m.insert($k, $v); )*
        m
    }};
}
