//! 稳定身份（stable identity）：所有身份都由内容/位置确定性派生，
//! 导出 -> 删除数据库 -> 再次导入后保持不变。

/// 64 位 FNV-1a。
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn digest(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        h ^= 0x1f;
        for &b in p.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{:016x}", h)
}

/// 材料身份：批次 + 批内序号 + 原始表示（raw 文本参与哈希，改动直接来源即改身份）。
pub fn material_id(batch_id: &str, ordinal: usize, raw: &str) -> String {
    format!("mat_{}", digest(&["material/v1", batch_id, &ordinal.to_string(), raw]))
}

/// 内容指纹（导入时记录，损坏检测使用）。
pub fn content_hash(raw: &str) -> String {
    format!("sha256-stub:{:016x}", fnv1a64(raw.as_bytes()))
}

/// 派生结果身份：输入摘要 + 规则版本 + 参数 + 父级依赖。
pub fn derived_id(inputs_digest: &str, rules_version: &str, params_digest: &str, parent: &str) -> String {
    format!(
        "der_{}",
        digest(&["derived/v1", inputs_digest, rules_version, params_digest, parent])
    )
}

/// 候选输出方案身份：派生结果 + 候选负载。
pub fn candidate_id(derived: &str, canonical_candidate: &str) -> String {
    format!("cand_{}", digest(&["candidate/v1", derived, canonical_candidate]))
}

/// 事件身份：分支 + 序号 + 类型 + 规范化负载。
pub fn event_id(branch: &str, ordinal: usize, etype: &str, payload_canonical: &str) -> String {
    format!(
        "evt_{}",
        digest(&["event/v1", branch, &ordinal.to_string(), etype, payload_canonical])
    )
}

pub fn branch_id(name: &str, parent: &str, fork_seq: usize) -> String {
    format!("br_{}", digest(&["branch/v1", name, parent, &fork_seq.to_string()]))
}

pub fn decision_id(branch: &str, ordinal: usize, target: &str, kind: &str) -> String {
    format!(
        "dec_{}",
        digest(&["decision/v1", branch, &ordinal.to_string(), target, kind])
    )
}

pub fn out_sst_id(derived: &str, index: usize) -> String {
    format!("sstout_{}", digest(&["sstout/v1", derived, &index.to_string()]))
}
