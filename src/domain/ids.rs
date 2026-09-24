//! 稳定身份与规范化序列化。
//!
//! 身份只由载荷决定（不含墙钟时间），因此等价输入在导出/导入、重放、半写恢复后
//! 仍然得到同一 ID。对象之间以 ID 形成可反查的来源关系。

use serde::Serialize;
use sha2::{Digest, Sha256};

/// 将任意可序列化值转换为“规范化 JSON”：对象键排序、无空白、数组保序。
pub fn canonical_json<T: Serialize>(value: &T) -> String {
    let json_value = serde_json::to_value(value).expect("domain values always serialize");
    canonicalize(&json_value)
}

fn canonicalize(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).unwrap(),
        serde_json::Value::Array(items) => {
            let mut out = String::from("[");
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&canonicalize(item));
            }
            out.push(']');
            out
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (idx, key) in keys.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap());
                out.push(':');
                out.push_str(&canonicalize(&map[key]));
            }
            out.push('}');
            out
        }
    }
}

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// 取 SHA-256 前 12 个十六进制字符（48 bit），配合前缀足以在案例规模内防碰撞。
pub fn short_digest(canonical: &str) -> String {
    sha256_hex(canonical)[..12].to_string()
}

/// 由类型前缀与规范化载荷生成稳定身份：`{prefix}_{12hex}`。
pub fn stable_id<T: Serialize>(prefix: &str, payload: &T) -> String {
    format!("{}_{}", prefix, short_digest(&canonical_json(payload)))
}
