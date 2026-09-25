//! 资源预算：输出条数 / 字节 / 合法候选数。
//! 预算耗尽是与“格式损坏”“语义无解”不同的第三种结果。

use crate::engine::error::{AppError, Diagnostic, Pos};
use crate::engine::model::SSTable;

#[derive(Debug, Clone)]
pub struct Budget {
    pub max_records: i64,
    pub max_bytes: Option<i64>,
    pub max_candidates: i64,
}

impl Budget {
    pub fn check_candidates(&self, n: usize, req_id: &str) -> Result<(), AppError> {
        if (n as i64) > self.max_candidates {
            return Err(AppError::ResourceExhausted {
                diagnostic: Diagnostic {
                    code: "D_BUDGET_TOO_MANY_CANDIDATES".into(),
                    message: format!(
                        "合法输出切分候选 {n} 超过候选预算 {}，无法在预算内唯一展开",
                        self.max_candidates
                    ),
                    pos: Pos { path: format!("compaction[{req_id}].split_candidates"), object_id: Some(req_id.into()) },
                },
            });
        }
        Ok(())
    }
}

/// 一组点记录的字节合计；任何不可转换大小都使字节预算“无法证明” => 资源耗尽。
pub fn group_bytes(points: &[&crate::engine::model::PointRecord]) -> Result<i64, AppError> {
    let mut total: i64 = 0;
    for p in points {
        let n = p
            .value
            .as_ref()
            .and_then(|v| v.decoded.as_ref().map(|s| s.as_bytes().len() as i64))
            .unwrap_or(0);
        total = total.checked_add(n).ok_or_else(|| {
            AppError::ResourceExhausted {
                diagnostic: Diagnostic {
                    code: "D_BUDGET_BYTE_OVERFLOW".into(),
                    message: format!("point {} 字节合计溢出", p.id),
                    pos: Pos { path: format!("points.{}", p.id), object_id: Some(p.id.clone()) },
                },
            }
        })?;
    }
    Ok(total)
}

/// SSTable 大小不可解释时（导入保留的原值无法转换），字节预算无法证明。
pub fn check_sst_sizes_usable(ssts: &[&SSTable], req_id: &str) -> Result<(), AppError> {
    for s in ssts {
        if s.size_bytes.normalized.is_none() {
            return Err(AppError::ResourceExhausted {
                diagnostic: Diagnostic {
                    code: "D_UNCONVERTIBLE_SIZE".into(),
                    message: format!(
                        "SSTable {} 的大小原始表示 {:?} 无法规范化为 byte，字节预算无法证明；原始值已保留在原位置",
                        s.id, s.size_bytes.raw_repr
                    ),
                    pos: Pos { path: format!("sstables[{}].size_bytes", s.id), object_id: Some(s.id.clone()) },
                },
            });
        }
    }
    let _ = req_id;
    Ok(())
}
