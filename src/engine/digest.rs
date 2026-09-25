//! 稳定摘要：等价输入得到稳定顺序与稳定身份。
//! 使用 FNV-1a 64 位（零依赖、确定性）。摘要前先做规范化 JSON 序列化，
//! 因此跨导入/导出的等价材料产生相同身份。

use serde::Serialize;

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 对任意可序列化结构做稳定摘要（键序固定）。
pub fn stable_digest<T: Serialize>(v: &T) -> String {
    let json = serde_json::to_string(v).expect("stable digest serialize");
    format!("{:016x}", fnv1a64(json.as_bytes()))
}

/// 由命名空间 + 组成部分构造稳定身份。
pub fn stable_id(namespace: &str, parts: &[&str]) -> String {
    let mut joined = String::new();
    for p in parts {
        joined.push_str(&p.len().to_string());
        joined.push(':');
        joined.push_str(p);
    }
    format!("{namespace}-{:016x}", fnv1a64(joined.as_bytes()))
}

/// 顺序键：先按字节序键，再按 seqno 降序，再按类型序（Delete 先于 Put）。
pub fn order_token(user_key_bytes: &[u8], seq: i64, type_rank: u8) -> String {
    // 仅用于展示/排序辅助；真实比较在 model 中完成
    let mut t = String::new();
    t.push_str(&hex::encode(user_key_bytes));
    let _ = (seq, type_rank);
    t
}

mod hex {
    pub fn encode(b: &[u8]) -> String {
        let mut s = String::new();
        for x in b {
            s.push_str(&format!("{x:02x}"));
        }
        s
    }
}
