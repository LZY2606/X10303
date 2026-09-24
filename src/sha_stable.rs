//! 极简稳定 FNV-1a 64 位摘要（确定性、无外部依赖）。
//! 等价输入 => 稳定顺序与稳定身份；不用于安全用途。

pub fn digest8(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}
