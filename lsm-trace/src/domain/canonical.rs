//! 规范化 JSON：对象按 key 排序，避免无意义空白差异，使摘要哈希稳定可重放。

use serde::Serialize;
use serde_json::Value;

/// 把任意可序列化值转成“规范字节”：对象键有序、紧凑分隔符。
/// 浮点在本系统不用于身份；数字统一按 JSON 数字比较（整数）。
pub fn canonical_bytes<T: Serialize>(v: &T) -> Vec<u8> {
    let value = serde_json::to_value(v).expect("infallible serialize to Value");
    let mut out = Vec::new();
    write_canonical(&value, &mut out);
    out
}

pub fn canonical_hex<T: Serialize>(v: &T) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(canonical_bytes(v));
    hex::encode(h.finalize())
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => {
            // serde_json::to_writer 负责转义。
            let _ = serde_json::to_writer(&mut *out, s);
        }
        Value::Array(arr) => {
            out.push(b'[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let _ = serde_json::to_writer(&mut *out, k.as_str());
                out.push(b':');
                write_canonical(&map[*k], out);
            }
            out.push(b'}');
        }
    }
}
