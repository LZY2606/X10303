//! 稳定身份：身份是规范化载荷的哈希，导出再导入后保持不变。
//! 事件身份还包含父事件链，形成可校验的哈希链。

use serde::Serialize;
use sha2::{Digest, Sha256};

pub fn digest12(payload: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"lsm-audit-v1\0");
    h.update(payload);
    hex::encode(&h.finalize()[..6])
}

/// 对可序列化载荷计算稳定 id（12 位 hex）。
pub fn id_of<T: Serialize>(tag: &str, payload: &T) -> String {
    let mut h = Sha256::new();
    h.update(b"lsm-audit-v1\0");
    h.update(tag.as_bytes());
    h.update(b"\0");
    h.update(canonical_json(payload).as_bytes());
    hex::encode(&h.finalize()[..6])
}

/// 事件链 id：parent_id + 规范化事件体。
pub fn event_id(parent_id: &str, payload: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(b"lsm-audit-event-v1\0");
    h.update(parent_id.as_bytes());
    h.update(b"\0");
    h.update(canonical_json(payload).as_bytes());
    hex::encode(&h.finalize()[..8])
}

/// 确定性 JSON（key 排序，无多余空白），保证等价输入稳定。
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
            let mut keys: Vec<_> = o.keys().collect();
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

/// 带原始表示与单位的尺寸解析：失败时返回诊断而不是默认值。
pub fn parse_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    let (num, mul) = if let Some(n) = s.strip_suffix("KiB") {
        (n.trim(), 1024u64)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n.trim(), 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("B") {
        (n.trim(), 1u64)
    } else {
        (s, 1u64)
    };
    num.parse::<u64>()
        .map(|n| n * mul)
        .map_err(|_| format!("无法把尺寸 {raw:?} 规范化为字节（单位基准: B/KiB/MiB）"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn canonical_order_stable() {
        let mut a = BTreeMap::new();
        a.insert("z", 1);
        a.insert("a", 2);
        let v = serde_json::json!({"a":2,"z":1});
        assert_eq!(canonical_json(&a), canonical_json(&v));
        assert_eq!(id_of("t", &a), id_of("t", &v));
    }
}
