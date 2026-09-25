//! 输出切分：只能在用户键边界切；在条数/字节预算内枚举全部合法切分。
//! 无法唯一决定时保留全部合法候选；候选数超预算 => 资源预算耗尽。

use crate::engine::error::AppError;
use crate::engine::model::PointRecord;
use serde::{Deserialize, Serialize};

/// 一段输出覆盖的用户键区间（闭区间语义展示，实际由包含记录决定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub first_user_key: String,
    pub last_user_key: String,
    pub point_count: i64,
    pub byte_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitPlan {
    /// 每段的点记录 id
    pub groups: Vec<Vec<String>>,
    pub segments: Vec<Segment>,
}

fn bytes_of(p: &PointRecord) -> i64 {
    p.value
        .as_ref()
        .and_then(|v| v.decoded.as_ref().map(|s| s.as_bytes().len() as i64))
        .unwrap_or(0)
}

/// 枚举全部合法的连续切分（按记录序列顺序切，切点只能落在不同用户键之间）。
/// 返回每组的点记录 id 列表。
pub fn enumerate_partitions(
    ordered_points: &[&PointRecord],
    max_records: i64,
    max_bytes: Option<i64>,
) -> Vec<Vec<Vec<String>>> {
    // 可切点 i：段 [start..i) 结束于 i，要求第 i-1 与第 i 条用户键不同。
    let n = ordered_points.len();
    let can_cut_before = |i: usize| -> bool {
        if i == 0 || i == n {
            return true;
        }
        ordered_points[i - 1].key.bytes() != ordered_points[i].key.bytes()
    };

    // 预计算合法段 [l..r)
    let mut valid = vec![vec![false; n + 1]; n + 1];
    for l in 0..n {
        let mut cnt: i64 = 0;
        let mut byt: i64 = 0;
        for r in (l + 1)..=n {
            cnt += 1;
            byt += bytes_of(ordered_points[r - 1]);
            let fit = cnt <= max_records && max_bytes.map_or(true, |mb| byt <= mb);
            // 段必须在用户键边界起止：起点 l 必须可切，终点 r 必须可切
            valid[l][r] = fit && can_cut_before(l) && can_cut_before(r);
            if cnt > max_records || max_bytes.map_or(false, |mb| byt > mb) {
                break;
            }
        }
    }

    // dp[i] = 覆盖前 i 条的全部切分（以段起点列表表示，不含 0、含 n）
    let mut dp: Vec<Vec<Vec<usize>>> = vec![Vec::new(); n + 1];
    dp[0] = vec![Vec::new()];
    for i in 1..=n {
        let mut acc: Vec<Vec<usize>> = Vec::new();
        for l in 0..i {
            if valid[l][i] && !dp[l].is_empty() {
                for prefix in &dp[l] {
                    let mut p = prefix.clone();
                    p.push(i);
                    acc.push(p);
                }
            }
        }
        dp[i] = acc;
    }

    dp[n]
        .iter()
        .map(|cuts| {
            let mut groups = Vec::new();
            let mut start = 0usize;
            for &e in cuts {
                let g: Vec<String> = ordered_points[start..e].iter().map(|p| p.id.clone()).collect();
                groups.push(g);
                start = e;
            }
            groups
        })
        .collect()
}

/// 生成采纳方案（默认：段数最少且最早切分的方案）。
pub fn adopted_plan(ordered_points: &[&PointRecord], groups: &[Vec<String>]) -> SplitPlan {
    let by_id: std::collections::BTreeMap<&str, &PointRecord> =
        ordered_points.iter().map(|p| (p.id.as_str(), *p)).collect();
    let mut segments = Vec::new();
    for g in groups {
        let first = by_id[g.first().unwrap().as_str()];
        let last = by_id[g.last().unwrap().as_str()];
        let mut byte_size = 0i64;
        for id in g {
            byte_size += bytes_of(by_id[id.as_str()]);
        }
        segments.push(Segment {
            first_user_key: first.key.logical.clone(),
            last_user_key: last.key.logical.clone(),
            point_count: g.len() as i64,
            byte_size,
        });
    }
    SplitPlan { groups: groups.to_vec(), segments }
}

/// 切点（每段首键）表示，便于 UI / 候选展示。
pub fn candidate_cuts(
    ordered_points: &[&PointRecord],
    max_records: i64,
    max_bytes: Option<i64>,
    max_candidates: i64,
) -> Result<(Vec<Vec<String>>, Vec<Vec<String>>), AppError> {
    let partitions = enumerate_partitions(ordered_points, max_records, max_bytes);
    if partitions.is_empty() {
        return Err(AppError::SemanticUnsolvable {
            diagnostic: crate::engine::error::Diagnostic {
                code: "D_SPLIT_IMPOSSIBLE_UNDER_BUDGET".into(),
                message: "不存在任何满足条数/字节预算且只在用户键边界切分的输出切分".into(),
                pos: crate::engine::error::Pos { path: "compaction.outputs".into(), object_id: None },
            },
            candidates: vec![],
        });
    }
    if partitions.len() as i64 > max_candidates {
        return Err(AppError::ResourceExhausted {
            diagnostic: crate::engine::error::Diagnostic {
                code: "D_BUDGET_TOO_MANY_CANDIDATES".into(),
                message: format!(
                    "合法输出切分候选 {} 超过候选预算 {max_candidates}",
                    partitions.len()
                ),
                pos: crate::engine::error::Pos { path: "compaction.outputs.candidates".into(), object_id: None },
            },
        });
    }
    // 排序：段数升序，然后按切点位置升序，保证等价输入稳定顺序
    let mut parts = partitions;
    parts.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    let adopted = parts.first().cloned().unwrap();
    let cuts = parts
        .iter()
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| g.first().cloned())
                .collect::<Vec<_>>()
                .iter()
                .filter_map(|pid| ordered_points.iter().find(|p| p.id == *pid).map(|p| p.key.logical.clone()))
                .collect()
        })
        .collect();
    Ok((adopted, cuts))
}
