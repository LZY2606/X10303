//! 稳定身份：所有身份都由规范化内容（含父依赖）哈希得到。
//! 等价输入 => 相同身份；身份不依赖数据库自增 id 或插入顺序。

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 递归地把 JSON 对象按键排序（数组保持顺序），输出无空白紧凑字节。
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => {
            // serde_json 会做标准 JSON 转义，保证字节确定。
            out.extend_from_slice(serde_json::to_vec(s).unwrap_or_default().as_slice());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
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
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_vec(key).unwrap_or_default().as_slice());
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
    }
}

pub fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 由规范化内容得到带前缀的 16 字符稳定身份。
/// `context` 用于区分不同名字空间（如 mat_ / evt_ / out_）。
pub fn stable_id(prefix: &str, content: &impl Serialize) -> String {
    let value = serde_json::to_value(content).expect("stable_id content must serialize");
    let hash = digest_hex(&canonical_bytes(&value));
    format!("{prefix}_{}", &hash[..16])
}

/// 任意字节的短指纹（用于导入档案整体指纹）。
pub fn fingerprint_bytes(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}_{}", &digest_hex(bytes)[..16])
}
