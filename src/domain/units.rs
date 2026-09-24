//! 单位与坐标基准。
//!
//! 领域量导入时保留原始表示（raw + 原单位），规范化只发生在派生层（normalized）。
//! 无法转换的值留在原位置并附诊断，绝不为了画图而丢弃。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantity {
    Temperature,
    Bytes,
    Count,
    CountPerSecond,
    Identity,
}

/// 坐标/时间基准：所有“量”都必须说明它相对什么、以什么单位计量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Basis {
    pub id: String,
    pub name: String,
    pub key_ordering: String,
    pub sequence_semantics: String,
    pub time_origin: String,
}

impl Basis {
    pub fn case_default() -> Self {
        Self {
            id: "basis_lsm_case_v1".to_string(),
            name: "层合迹案例坐标基准 v1".to_string(),
            key_ordering: "user_key 按 UTF-8 字节序升序；同键按 (seq 降序, type 降序)".to_string(),
            sequence_semantics: "sequence number 全局单调递增；snapshot 持有序号 S 时可见 seq<=S".to_string(),
            time_origin: "epoch_ms 相对 UNIX epoch 毫秒，仅作叙述展示，不参与身份与排序".to_string(),
        }
    }
}

/// 带单位的领域量。`raw` 与 `raw_unit` 是不可变材料；`normalized` 是派生层。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    pub raw: serde_json::Value,
    pub raw_unit: String,
    pub quantity: Quantity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub normalized_unit: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<String>,
}

impl Measured {
    /// 规范化：成功填 `normalized`；失败则原值留在 `raw`，诊断挂在 `diagnostics`。
    pub fn new(raw: serde_json::Value, raw_unit: impl Into<String>, quantity: Quantity) -> Self {
        let raw_unit = raw_unit.into();
        let mut measured = Self {
            raw,
            raw_unit,
            quantity,
            normalized: None,
            normalized_unit: None,
            diagnostics: Vec::new(),
        };
        measured.normalize();
        measured
    }

    fn normalize(&mut self) {
        match self.quantity {
            Quantity::Identity => {
                self.normalized = None;
                self.normalized_unit = None;
            }
            Quantity::Temperature => {
                let value = self.raw.as_f64();
                match (value, self.raw_unit.as_str()) {
                    (Some(v), "C" | "°C" | "celsius") => {
                        self.normalized = Some(v);
                        self.normalized_unit = Some("C".to_string());
                    }
                    (Some(v), "K" | "kelvin") => {
                        self.normalized = Some(v - 273.15);
                        self.normalized_unit = Some("C".to_string());
                    }
                    (Some(_), other) => self.diagnostics.push(format!(
                        "temperature 无法从单位 {other:?} 转换到基准 C；原值保留在 raw"
                    )),
                    (None, _) => self
                        .diagnostics
                        .push("temperature 的 raw 不是数值；原值保留在 raw".to_string()),
                }
            }
            Quantity::Bytes => {
                let value = self.raw.as_f64();
                match (value, self.raw_unit.as_str()) {
                    (Some(v), "B" | "byte") => {
                        self.normalized = Some(v);
                        self.normalized_unit = Some("B".to_string());
                    }
                    (Some(v), "KiB") => {
                        self.normalized = Some(v * 1024.0);
                        self.normalized_unit = Some("B".to_string());
                    }
                    (Some(v), "MiB") => {
                        self.normalized = Some(v * 1024.0 * 1024.0);
                        self.normalized_unit = Some("B".to_string());
                    }
                    (Some(_), other) => self.diagnostics.push(format!(
                        "bytes 无法从单位 {other:?} 转换到基准 B；原值保留在 raw"
                    )),
                    (None, _) => self
                        .diagnostics
                        .push("bytes 的 raw 不是数值；原值保留在 raw".to_string()),
                }
            }
            Quantity::Count => {
                let value = self.raw.as_i64();
                match (value, self.raw_unit.as_str()) {
                    (Some(v), "entry" | "entries" | "count") => {
                        self.normalized = Some(v as f64);
                        self.normalized_unit = Some("entry".to_string());
                    }
                    (Some(_), other) => self.diagnostics.push(format!(
                        "count 无法从单位 {other:?} 转换到基准 entry；原值保留在 raw"
                    )),
                    (None, _) => self
                        .diagnostics
                        .push("count 的 raw 不是整数；原值保留在 raw".to_string()),
                }
            }
            Quantity::CountPerSecond => {
                let value = self.raw.as_f64();
                match (value, self.raw_unit.as_str()) {
                    (Some(v), "entries/s" | "entry/s" | "eps") => {
                        self.normalized = Some(v);
                        self.normalized_unit = Some("entries/s".to_string());
                    }
                    (Some(v), "entries/min") => {
                        self.normalized = Some(v / 60.0);
                        self.normalized_unit = Some("entries/s".to_string());
                    }
                    (Some(_), other) => self.diagnostics.push(format!(
                        "rate 无法从单位 {other:?} 转换到基准 entries/s；原值保留在 raw"
                    )),
                    (None, _) => self
                        .diagnostics
                        .push("rate 的 raw 不是数值；原值保留在 raw".to_string()),
                }
            }
        }
    }

    /// 展示字符串：优先规范化值（带基准单位），否则退回原始表示。
    pub fn display(&self) -> String {
        if let Some(n) = self.normalized {
            format!("{} {}", trim_number(n), self.normalized_unit.clone().unwrap_or_default())
        } else {
            format!("{} {}", self.raw, self.raw_unit)
        }
    }
}

fn trim_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{:.3}", value).trim_end_matches('0').trim_end_matches('.').to_string()
    }
}
