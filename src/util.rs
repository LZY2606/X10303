//! 稳定身份工具：输入摘要（规范化 JSON + SHA-256）。

use serde::Serialize;
use sha2::{Digest, Sha256};

/// 等价输入得到稳定摘要：先按值序列化成规范 JSON（serde_json 保留 BTreeMap 键序）。
pub fn canonical_json<T: Serialize>(v: &T) -> String {
    let val = serde_json::to_value(v).expect("serialize");
    canonicalize(&val)
}

fn canonicalize(v: &serde_json::Value) -> String {
    serde_json::to_string(&sort_value(v)).unwrap()
}

fn sort_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = m.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let o: serde_json::Map<String, serde_json::Value> = entries.into_iter()
                .map(|(k, val)| (k.clone(), sort_value(val))).collect();
            serde_json::Value::Object(o)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(sort_value).collect())
        }
        x => x.clone(),
    }
}

pub fn digest_hex<T: Serialize>(v: &T) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(v).as_bytes());
    hex::encode(h.finalize())
}
