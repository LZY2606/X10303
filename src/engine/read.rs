//! 读取路径：按内部键顺序（user key 升序、seqno 降序、Delete 先于 Put）
//! 与 snapshot 判可见性；range tombstone 不越过未参与压实的旧数据提前丢弃。

use crate::engine::model::{PointRecord, RangeTomb, RecordKind};
use crate::engine::model::cmp_internal;
use crate::engine::{AppError, Diagnostic, KeyRep, Pos};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ReadVerdict {
    NotFound,
    Deleted {
        by_id: String,
        by_kind: String,
        seqno: i64,
    },
    Value {
        point_id: String,
        seqno: i64,
        value_raw: String,
    },
}

/// 版本链上一个候选的判定依据（机器可读）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainStep {
    pub point_id: String,
    pub seqno: i64,
    pub kind: RecordKind,
    /// snapshot 下是否可见（seqno <= snapshot；无 snapshot 恒可见）
    pub visible: bool,
    /// 被哪条 range tombstone 掩蔽（range id）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub masked_by_range: Option<String>,
    /// 判定原因代码
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadReport {
    pub key_logical: String,
    /// 读取快照 seqno；None 表示最新读
    pub snapshot_seqno: Option<i64>,
    pub chain: Vec<ChainStep>,
    /// 覆盖该键、且在 snapshot 下可见的 range tombstone（按 seqno 降序）
    pub covering_ranges: Vec<String>,
    pub verdict: ReadVerdict,
}

struct PointSrc<'a> {
    rec: &'a PointRecord,
}

/// `points` 为所有仍生效文件/memtable 中该键的 point 记录；
/// `ranges` 为所有仍生效文件/memtable 中的 range tombstone。
pub fn read_point(
    key: &KeyRep,
    snapshot_seqno: Option<i64>,
    points: &[&PointRecord],
    ranges: &[&RangeTomb],
) -> Result<ReadReport, AppError> {
    for p in points {
        if !p.usable() {
            return Err(AppError::Corrupt {
                diagnostic: Diagnostic {
                    code: "D_CORRUPT_UNUSABLE_POINT".into(),
                    message: format!("point {} 带无法解释的诊断，不能参与读取", p.id),
                    pos: Pos { path: format!("points[{}]", p.id), object_id: Some(p.id.clone()) },
                },
            });
        }
    }

    let kb = key.bytes();
    // 覆盖且可见的 range tombstone（按 seqno 降序）
    let mut covering: Vec<&&RangeTomb> = ranges
        .iter()
        .copied()
        .filter(|r| r.covers(key) && snapshot_seqno.map_or(true, |s| r.seq() <= s))
        .collect();
    covering.sort_by(|a, b| b.seq().cmp(&a.seq()));
    let newest_range_seq = covering.first().map(|r| r.seq());
    let covering_ranges = covering.iter().map(|r| r.id.clone()).collect();

    // 候选 point 记录按内部键顺序排序（同键 => seqno 降序、Delete 先）
    let mut ordered: Vec<PointSrc> = points
        .iter()
        .copied()
        .filter(|p| p.key.bytes() == kb)
        .map(|rec| PointSrc { rec })
        .collect();
    ordered.sort_by(|x, y| {
        cmp_internal(
            &x.rec.key.bytes(), x.rec.seq(), x.rec.kind,
            &y.rec.key.bytes(), y.rec.seq(), y.rec.kind,
        )
    });

    let mut chain = Vec::new();
    let mut verdict = ReadVerdict::NotFound;

    for s in &ordered {
        let p = s.rec;
        let visible = snapshot_seqno.map_or(true, |snap| p.seq() <= snap);
        if !visible {
            chain.push(ChainStep {
                point_id: p.id.clone(),
                seqno: p.seq(),
                kind: p.kind,
                visible: false,
                masked_by_range: None,
                reason: "R_AFTER_SNAPSHOT".into(),
            });
            continue;
        }
        let masked_by = newest_range_seq
            .filter(|rs| p.seq() <= *rs)
            .and_then(|_| covering.first().map(|r| r.id.clone()));

        if let Some(rid) = &masked_by {
            chain.push(ChainStep {
                point_id: p.id.clone(),
                seqno: p.seq(),
                kind: p.kind,
                visible: true,
                masked_by_range: Some(rid.clone()),
                reason: "R_MASKED_BY_RANGE_TOMB".into(),
            });
            continue;
        }

        match p.kind {
            RecordKind::Delete => {
                chain.push(ChainStep {
                    point_id: p.id.clone(),
                    seqno: p.seq(),
                    kind: p.kind,
                    visible: true,
                    masked_by_range: None,
                    reason: "R_POINT_DELETE".into(),
                });
                verdict = ReadVerdict::Deleted {
                    by_id: p.id.clone(),
                    by_kind: "point".into(),
                    seqno: p.seq(),
                };
                break;
            }
            RecordKind::Put => {
                chain.push(ChainStep {
                    point_id: p.id.clone(),
                    seqno: p.seq(),
                    kind: p.kind,
                    visible: true,
                    masked_by_range: None,
                    reason: "R_VISIBLE_PUT".into(),
                });
                verdict = ReadVerdict::Value {
                    point_id: p.id.clone(),
                    seqno: p.seq(),
                    value_raw: p.value.as_ref().map(|v| v.raw_repr.clone()).unwrap_or_default(),
                };
                break;
            }
        }
    }

    if matches!(verdict, ReadVerdict::NotFound) {
        if let Some(r) = covering.first() {
            verdict = ReadVerdict::Deleted {
                by_id: r.id.clone(),
                by_kind: "range".into(),
                seqno: r.seq(),
            };
        }
    }

    Ok(ReadReport {
        key_logical: key.logical.clone(),
        snapshot_seqno,
        chain,
        covering_ranges,
        verdict,
    })
}
