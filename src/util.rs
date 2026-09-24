//! 字节序坐标工具与稳定身份（内容哈希）。

use serde::Serialize;

/// 极简稳定 FNV-1a 64 位摘要（确定性、无外部依赖）。
/// 等价输入 => 稳定身份；不用于安全用途。
pub fn digest8(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// 规范化 hex（小写，偶数长度）。
pub fn norm_hex(s: &str) -> Option<String> {
    let t = s.trim().trim_start_matches("0x");
    if t.is_empty() || t.len() % 2 != 0 {
        return None;
    }
    let mut out = String::with_capacity(t.len());
    for c in t.chars() {
        if c.is_ascii_hexdigit() {
            out.push(c.to_ascii_lowercase());
        } else {
            return None;
        }
    }
    Some(out)
}

pub fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let h = norm_hex(s)?;
    let mut out = Vec::with_capacity(h.len() / 2);
    let b = h.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

pub fn bytes_to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

/// 字节序字典序比较。
pub fn cmp_hex(a: &str, b: &str) -> std::cmp::Ordering {
    let ba = hex_to_bytes(a).unwrap_or_default();
    let bb = hex_to_bytes(b).unwrap_or_default();
    ba.cmp(&bb)
}

/// 固定宽度 key 的后继（用于把闭区间 end 转为半开边界 end+1）。
pub fn successor_hex(s: &str) -> Option<String> {
    let mut b = hex_to_bytes(s)?;
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        if b[i] != 0xff {
            b[i] += 1;
            return Some(bytes_to_hex(&b));
        }
    }
    None
}

pub fn min_hex<'a>(a: &'a str, b: &'a str) -> &'a str {
    if cmp_hex(a, b).is_le() { a } else { b }
}

pub fn max_hex<'a>(a: &'a str, b: &'a str) -> &'a str {
    if cmp_hex(a, b).is_ge() { a } else { b }
}

/// 闭区间包含。
pub fn contains_closed(range: (&str, &str), k: &str) -> bool {
    cmp_hex(range.0, k).is_le() && cmp_hex(k, range.1).is_le()
}

/// 半开区间 [start,end) 包含。
pub fn contains_half(start: &str, end: &str, k: &str) -> bool {
    cmp_hex(start, k).is_le() && cmp_hex(k, end).is_lt()
}

/// 两个半开区间是否非空相交。
pub fn half_overlaps(a: (&str, &str), b: (&str, &str)) -> bool {
    cmp_hex(a.0, b.1).is_lt() && cmp_hex(b.0, a.1).is_lt()
}

/// 把半开区间 [hs,he) 裁剪到闭区间 [cs,ce] 内。
pub fn clamp_half_to_closed(half: (&str, &str), closed: (&str, &str)) -> Option<(String, String)> {
    let (hs, he) = half;
    let (cs, ce) = closed;
    let s = max_hex(hs, cs);
    let e = match successor_hex(ce) {
        Some(n) => min_hex(he, &n),
        None => he,
    };
    if cmp_hex(s, e).is_lt() {
        Some((s.to_string(), e.to_string()))
    } else {
        None
    }
}

/// 两个半开区间求交。
pub fn clamp_half_to_half(a: (&str, &str), b: (&str, &str)) -> Option<(String, String)> {
    let s = max_hex(a.0, b.0);
    let e = min_hex(a.1, b.1);
    if cmp_hex(s, e).is_lt() {
        Some((s.to_string(), e.to_string()))
    } else {
        None
    }
}

/// 估算条目大小（bytes）：key + value 字节 + 固定内部键开销（seq+type）。
pub fn point_size_bytes(key_hex: &str, value_hex: Option<&str>) -> u64 {
    let kb = hex_to_bytes(key_hex).map(|b| b.len()).unwrap_or(0) as u64;
    let vb = value_hex.and_then(hex_to_bytes).map(|b| b.len() as u64).unwrap_or(0);
    kb + vb + 16
}

/// 对可序列化结构计算 8 字节（16 hex 字符）稳定摘要。
pub fn stable_id(prefix: &str, v: &impl Serialize) -> String {
    let json = serde_json::to_string(v).expect("stable_id serialize");
    format!("{}:{}", prefix, digest8(&json))
}

/// 两闭区间是否不重叠（a 完全在 b 前，可相邻）。
pub fn disjoint_closed(a: (&str, &str), b: (&str, &str)) -> bool {
    cmp_hex(a.1, b.0).is_lt()
}
