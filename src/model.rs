//! 不可变材料模型 + 带单位/坐标基准的领域量。
//!
//! 约定：导入时保留原始表示（`raw`），只在派生层规范化（`normalized`）。
//! 无法转换的值留在原位置并附 [`Diagnostic`]，绝不为了画图丢数据，也不用默认值伪装成功。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 三种（业务）失败结果必须彼此不同，外加成功，互不混淆。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineOutcome<T> {
    /// 计算完成（可能仍携带非致命诊断，例如保留的合法候选）。
    Success { value: T, diagnostics: Vec<Diagnostic> },
    /// 资源预算耗尽：条目数/输出字节数/分裂数等预算先于语义完成耗尽。
    BudgetExhausted { state: T, budget: BudgetReport, at: String },
    /// 格式损坏：输入材料内部不自洽，无法作为 LSM 材料解释。
    FormatCorrupt { diagnostics: Vec<Diagnostic> },
    /// 语义无解：材料自洽但规则下无法唯一决定（如同序号多类型冲突）。
    SemanticUnresolvable { value: T, candidates: Vec<String>, reason: String, diagnostics: Vec<Diagnostic> },
}

impl<T> EngineOutcome<T> {
    pub fn ok(value: T) -> Self {
        EngineOutcome::Success { value, diagnostics: Vec::new() }
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            EngineOutcome::Success { diagnostics, .. }
            | EngineOutcome::SemanticUnresolvable { diagnostics, .. }
            | EngineOutcome::FormatCorrupt { diagnostics } => diagnostics,
            EngineOutcome::BudgetExhausted { .. } => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReport {
    pub budget: u64,
    pub used: u64,
    pub unit: String,
}

/// 带位置的诊断。`path` 为 JSON pointer 或领域坐标（如 `ssts/SST-a/ranges[0]/end`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub path: String,
    pub message: String,
    /// unconverted | ambiguous | corrupt | info
    pub severity: String,
}

impl Diagnostic {
    pub fn unconverted(path: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic { code: "UNCONVERTED_VALUE".into(), path: path.into(), message: message.into(), severity: "unconverted".into() }
    }
    pub fn ambiguous(path: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic { code: "AMBIGUOUS".into(), path: path.into(), message: message.into(), severity: "ambiguous".into() }
    }
    pub fn corrupt(path: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic { code: "FORMAT_CORRUPT".into(), path: path.into(), message: message.into(), severity: "corrupt".into() }
    }
    pub fn info(path: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic { code: "INFO".into(), path: path.into(), message: message.into(), severity: "info".into() }
    }
}

/// 领域量：原始表示始终保留；`normalized` 是派生层规范化结果（含统一单位/基准）。
///
/// 例如 `"2 KiB"` 规范化为 2048 byte；`"2K"` 这种本案例包无法解释的写法
/// 保留在 `raw` 中，`normalized = None` 并附诊断，而不是静默当 2048 或 0。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Q {
    pub raw: serde_json::Value,
    pub normalized: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
}

impl PartialEq for Q {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw && self.normalized == other.normalized && self.unit == other.unit
    }
}
impl Eq for Q {}

impl Q {
    pub fn from_int(v: i64, unit: &str) -> Self {
        Q { raw: serde_json::json!(v), normalized: Some(v), unit: Some(unit.into()), diagnostics: vec![] }
    }
    pub fn seq(v: i64) -> Self { Self::from_int(v, "seq") }
    pub fn level(v: i64) -> Self { Self::from_int(v, "level") }
    pub fn bytes(v: i64) -> Self { Self::from_int(v, "byte") }

    /// 解析字节量。支持数字（默认 byte）与 `B/byte, KiB/MiB/GiB, KB/MB/GB` 后缀。
    pub fn parse_bytes(raw: serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>) -> Self {
        let mut q = Q { raw: raw.clone(), normalized: None, unit: Some("byte".into()), diagnostics: vec![] };
        match &raw {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    q.normalized = Some(i.max(0));
                } else {
                    diag.push(Diagnostic::unconverted(path, format!("字节量不是整数: {n}")));
                    q.diagnostics.push(Diagnostic::unconverted(path, "非整数"));
                }
            }
            serde_json::Value::String(s) => {
                let s = s.trim();
                if let Some((num, unit)) = split_quantity(s) {
                    if let Ok(v) = num.parse::<f64>() {
                        match unit.to_ascii_lowercase().as_str() {
                            "b" | "byte" | "bytes" => q.normalized = Some(v as i64),
                            "kib" | "kibibyte" | "kibibytes" => q.normalized = Some((v * 1024.0) as i64),
                            "mib" | "mebibyte" | "mebibytes" => q.normalized = Some((v * 1024.0 * 1024.0) as i64),
                            "gib" => q.normalized = Some((v * 1024.0 * 1024.0 * 1024.0) as i64),
                            "kb" | "kilobyte" => q.normalized = Some((v * 1000.0) as i64),
                            "mb" | "megabyte" => q.normalized = Some((v * 1_000_000.0) as i64),
                            "gb" | "gigabyte" => q.normalized = Some((v * 1_000_000_000.0) as i64),
                            other => {
                                let d = Diagnostic::unconverted(path, format!("无法转换的字节单位 '{other}'（保留原始表示）"));
                                diag.push(d.clone());
                                q.diagnostics.push(d);
                                q.unit = Some(other.into());
                            }
                        }
                    } else {
                        let d = Diagnostic::unconverted(path, format!("无法解析的数量 '{num}'"));
                        diag.push(d.clone());
                        q.diagnostics.push(d);
                    }
                } else {
                    let d = Diagnostic::unconverted(path, format!("无法拆分的数量字符串 '{s}'"));
                    diag.push(d.clone());
                    q.diagnostics.push(d);
                }
            }
            other => {
                let d = Diagnostic::unconverted(path, format!("字节量类型不支持: {other}"));
                diag.push(d.clone());
                q.diagnostics.push(d);
            }
        }
        q
    }
}

fn split_quantity(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(|c: char| c.is_alphabetic())?;
    Some((s[..idx].trim(), s[idx..].trim()))
}

/// 稳定身份：同一内容永远得到同一 id（等价输入 -> 稳定顺序/稳定身份）。
pub fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"\x1f");
    }
    format!("{prefix}-{}", &hex8(&h.finalize()))
}

pub fn hex8(b: &[u8]) -> String {
    b.iter().take(4).map(|x| format!("{x:02x}")).collect()
}

/// 十六进制/原始 key（坐标基准：字节序 memcmp）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HexKey(pub String);

impl HexKey {
    pub fn new(s: impl Into<String>) -> Self { HexKey(s.into()) }
    pub fn as_str(&self) -> &str { &self.0 }
    pub fn bytes(&self) -> Vec<u8> { hex_decode(&self.0) }
}

/// 允许偶数长度的纯 hex（空串合法）；非法 hex 返回 None（格式损坏由调用方附位置）。
pub fn hex_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16);
        let lo = (b[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return vec![],
        }
        i += 2;
    }
    out
}

pub fn is_valid_hex(s: &str) -> bool {
    s.len() % 2 == 0 && s.bytes().all(|b| (b as char).is_ascii_hexdigit()) && (!s.is_empty() || true)
}

// ---------------------------------------------------------------------------
// 不可变材料：point value、删除记录、WAL、memtable flush、SSTable 摘要、snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Put,
    Delete,
}

/// point value / point tombstone 的原始记录（导入材料）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointRecord {
    pub id: String,
    pub seq: Q,
    pub key: HexKey,
    pub kind: PointKind,
    /// PUT 时的原始 value（字符串/十六进制/base64 等原始表示原样保留）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<RawValue>,
    /// 该条目出现在哪（wal 日志 id、flush 批次 id 或 sst 文件 id）。
    pub origin: RecordOrigin,
}

/// value 的原始表示 + 坐标基准；派生层才做解码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawValue {
    pub representation: String, // "utf8" | "hex" | "base64" | "json"
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum RecordOrigin {
    Wal { wal_id: String },
    Flush { flush_id: String },
    Sst { sst_id: String },
}

impl RecordOrigin {
    pub fn container_id(&self) -> &str {
        match self {
            RecordOrigin::Wal { wal_id } => wal_id,
            RecordOrigin::Flush { flush_id } => flush_id,
            RecordOrigin::Sst { sst_id } => sst_id,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            RecordOrigin::Wal { .. } => "wal",
            RecordOrigin::Flush { .. } => "flush",
            RecordOrigin::Sst { .. } => "sst",
        }
    }
}

/// 范围删除记录（range tombstone）。端点坐标基准为字节序；end 默认独占。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeTombstone {
    pub id: String,
    pub seq: Q,
    pub start: HexKey,
    pub end: HexKey,
    /// 端点语义：[start,end) 独占（exclusive）或 [start,end] 双闭（inclusive）。
    /// 这是“范围删除端点”情景直接改动的唯一来源字段。
    #[serde(default = "default_end_inclusive")]
    pub end_inclusive: bool,
    pub origin: RecordOrigin,
}

fn default_end_inclusive() -> bool { false }

/// SSTable key range（坐标基准：字节序 memcmp）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRange {
    pub smallest: HexKey,
    pub largest: HexKey,
}

/// WAL 条目摘要（导入后即为不可变材料）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry {
    pub id: String,
    pub points: Vec<PointRecord>,
    pub ranges: Vec<RangeTombstone>,
    /// 逻辑时间基准（wall-clock 原始表示，规范化为 epoch millis，无法转换则留诊断）。
    pub wall_time: Q,
}

/// memtable flush 摘要：把一批 WAL 条目刷成一个 L0 SSTable 的叙事纽带。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushSummary {
    pub id: String,
    pub memtable_id: String,
    pub wal_entries: Vec<String>,
    pub produced_sst: String,
    pub points: Vec<PointRecord>,
    pub ranges: Vec<RangeTombstone>,
    pub wall_time: Q,
}

/// 多层 SSTable 摘要：key range、level、point value、删除记录全部保存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SstSummary {
    pub id: String,
    pub level: Q,
    pub key_range: KeyRange,
    pub points: Vec<PointRecord>,
    pub ranges: Vec<RangeTombstone>,
    /// 文件大小原始表示（派生层规范化为 byte；无法转换留在原位）。
    pub file_size: Q,
    /// 来源：flush / compaction 输出 / 导入即存在。
    pub provenance: SstProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SstProvenance {
    Imported,
    Flush { flush_id: String },
    CompactionOutput {
        compaction_id: String,
        output_index: usize,
        input_ssts: Vec<String>,
    },
}

/// snapshot：可见性基准。`last_seq` 坐标基准与 sequence number 相同（seq）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub last_seq: Q,
    /// 审阅者标注的是否仍活跃；活跃旧 snapshot 会阻止旧版本/墓碑被提前丢弃。
    pub active: bool,
    pub label: String,
}

/// 压实参数（参数修订产生新版本，旧决定仍引用旧版本，绝不被新计算覆盖）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionParams {
    pub version: String,
    /// 同 key 同序号多类型时的裁决策略；v1 为“无解”，v2 为“delete 胜”。
    pub same_seq_policy: String,
    /// 单个输出文件字节预算（超过则在 key 边界切分）。
    pub output_bytes_budget: Q,
    /// 每次压实允许处理的条目预算（用于“资源预算耗尽/压实中断”）。
    pub entry_budget: Q,
    /// 输出最多切分片数。
    pub max_outputs: u64,
    /// 是否允许在 L0 选择时做传递性重叠扩张。
    pub expand_l0_overlap: bool,
}

/// 离线案例包：“层合迹：LSM tombstone、范围删除与压实选择审阅”的领域材料，
/// 不是通用表格。导入后全部成为不可变材料。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasePack {
    pub case_id: String,
    pub schema_version: String,
    pub title: String,
    pub narrative: String,
    pub snapshots: Vec<Snapshot>,
    pub wal: Vec<WalEntry>,
    pub flushes: Vec<FlushSummary>,
    pub ssts: Vec<SstSummary>,
    pub params: CompactionParams,
    /// 初始导入时一并保存的人工决定（含一次“互相冲突的人工决定”）。
    #[serde(default)]
    pub decisions: Vec<SeedDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedDecision {
    pub id: String,
    pub branch: String,
    pub summary: String,
    /// accept | reject | tune
    pub decision: String,
    pub target_compaction: Option<String>,
    pub conflicts_with: Option<String>,
    pub note: String,
}

// ---------------------------------------------------------------------------
// 导入：手工按 JSON pointer 校验。输入不完整 -> 带位置诊断（FormatCorrupt），
// 无法转换的值留在原位（unconverted），不用空值/默认值伪装成功。
// ---------------------------------------------------------------------------

pub struct Imported {
    pub pack: CasePack,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn import_case(raw: &serde_json::Value) -> Result<Imported, Vec<Diagnostic>> {
    let mut diag = Vec::new();
    let mut fatal = Vec::new();
    let obj = raw.as_object().ok_or_else(|| {
        vec![Diagnostic::corrupt("", "案例包顶层必须是对象")]
    })?;

    let case_id = req_str(obj, "case_id", "", &mut fatal);
    let title = req_str(obj, "title", "", &mut fatal);
    let narrative = req_str(obj, "narrative", "", &mut fatal);
    let schema_version = opt_str(obj, "schema_version").unwrap_or_else(|| "case-pack/v1".into());

    let snapshots = parse_array(raw, "snapshots", &mut fatal)
        .map(|a| a.iter().enumerate().flat_map(|(i, v)| parse_snapshot(v, &format!("/snapshots/{i}"), &mut diag, &mut fatal)).collect())
        .unwrap_or_default();
    let wal = parse_array(raw, "wal", &mut fatal)
        .map(|a| a.iter().enumerate().flat_map(|(i, v)| parse_wal(v, &format!("/wal/{i}"), &mut diag, &mut fatal)).collect())
        .unwrap_or_default();
    let flushes = parse_array(raw, "flushes", &mut fatal)
        .map(|a| a.iter().enumerate().flat_map(|(i, v)| parse_flush(v, &format!("/flushes/{i}"), &mut diag, &mut fatal)).collect())
        .unwrap_or_default();
    let ssts = parse_array(raw, "ssts", &mut fatal)
        .map(|a| a.iter().enumerate().flat_map(|(i, v)| parse_sst(v, &format!("/ssts/{i}"), &mut diag, &mut fatal)).collect())
        .unwrap_or_default();

    let params = match obj.get("params") {
        Some(v) => parse_params(v, "/params", &mut diag, &mut fatal).unwrap_or_else(default_params),
        None => {
            fatal.push(Diagnostic::corrupt("/params", "缺少压实参数 params（不得用默认参数伪装成功）"));
            default_params()
        }
    };
    let decisions = parse_array(raw, "decisions", &mut diag)
        .map(|a| a.iter().enumerate().flat_map(|(i, v)| parse_seed_decision(v, &format!("/decisions/{i}"), &mut diag, &mut fatal)).collect())
        .unwrap_or_default();

    if !fatal.is_empty() {
        return Err(fatal);
    }

    // 交叉引用与结构自洽性检查（格式损坏，而非语义无解）。
    let sst_ids: std::collections::BTreeSet<&str> = ssts.iter().map(|s| s.id.as_str()).collect();
    for f in &flushes {
        if !sst_ids.contains(f.produced_sst.as_str()) {
            fatal.push(Diagnostic::corrupt(
                &format!("/flushes/{}/produced_sst", f.id),
                format!("flush {} 产出的 SSTable {} 不存在", f.id, f.produced_sst),
            ));
        }
    }
    for sst in &ssts {
        if !is_valid_hex(sst.key_range.smallest.as_str()) || !is_valid_hex(sst.key_range.largest.as_str()) {
            fatal.push(Diagnostic::corrupt(&format!("/ssts/{}/key_range", sst.id), "key range 不是合法偶数长度 hex"));
        }
        if sst.key_range.smallest > sst.key_range.largest {
            fatal.push(Diagnostic::corrupt(&format!("/ssts/{}/key_range", sst.id), "smallest > largest（字节序不自洽）"));
        }
        for p in &sst.points {
            if p.origin.container_id() != sst.id {
                fatal.push(Diagnostic::corrupt(&format!("/ssts/{}/points/{}", sst.id, p.id), "point origin 与所在 SSTable 不一致"));
            }
        }
    }
    if !fatal.is_empty() {
        return Err(fatal);
    }

    Ok(Imported {
        pack: CasePack {
            case_id,
            schema_version,
            title,
            narrative,
            snapshots,
            wal,
            flushes,
            ssts,
            params,
            decisions,
        },
        diagnostics: diag,
    })
}

fn default_params() -> CompactionParams {
    CompactionParams {
        version: "__invalid__".into(),
        same_seq_policy: "unresolvable".into(),
        output_bytes_budget: Q::bytes(0),
        entry_budget: Q::seq(0),
        max_outputs: 0,
        expand_l0_overlap: false,
    }
}

fn req_str(obj: &serde_json::Map<String, serde_json::Value>, key: &str, path: &str, fatal: &mut Vec<Diagnostic>) -> String {
    match obj.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/{key}"), format!("缺少必填字符串字段 {key}")));
            String::new()
        }
    }
}

fn opt_str(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn parse_array<'a>(raw: &'a serde_json::Value, key: &str, fatal: &mut Vec<Diagnostic>) -> Option<&'a Vec<serde_json::Value>> {
    match raw.get(key) {
        Some(serde_json::Value::Array(a)) => Some(a),
        Some(_) => {
            fatal.push(Diagnostic::corrupt(&format!("/{key}"), format!("{key} 必须是数组")));
            None
        }
        None => {
            fatal.push(Diagnostic::corrupt(&format!("/{key}"), format!("缺少必填数组 {key}")));
            None
        }
    }
}

fn parse_snapshot(v: &serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<Snapshot> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let label = req_str(o, "label", path, fatal);
    let last_seq = match o.get("last_seq") {
        Some(serde_json::Value::Number(n)) if n.is_i64() || n.is_u64() => Q::seq(n.as_i64().unwrap_or(-1)),
        Some(other) => {
            let d = Diagnostic::unconverted(&format!("{path}/last_seq"), format!("snapshot last_seq 无法作为 seq 解释: {other}"));
            diag.push(d.clone());
            let mut q = Q { raw: other.clone(), normalized: None, unit: Some("seq".into()), diagnostics: vec![d] };
            q.normalized = None;
            fatal.push(Diagnostic::corrupt(&format!("{path}/last_seq"), "snapshot last_seq 必须是整数 seq"));
            q
        }
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/last_seq"), "缺少 last_seq"));
            Q::seq(-1)
        }
    };
    let active = o.get("active").and_then(|a| a.as_bool()).unwrap_or_else(|| {
        fatal.push(Diagnostic::corrupt(&format!("{path}/active"), "缺少 active 布尔值"));
        false
    });
    Some(Snapshot { id, last_seq, active, label })
}

fn parse_origin(v: Option<&serde_json::Value>, path: &str, fatal: &mut Vec<Diagnostic>) -> RecordOrigin {
    let Some(v) = v else {
        fatal.push(Diagnostic::corrupt(path, "缺少记录来源 origin"));
        return RecordOrigin::Wal { wal_id: String::new() };
    };
    let o = match v.as_object() {
        Some(o) => o,
        None => {
            fatal.push(Diagnostic::corrupt(path, "origin 必须是对象"));
            return RecordOrigin::Wal { wal_id: String::new() };
        }
    };
    let kind = o.get("origin").and_then(|x| x.as_str()).unwrap_or("");
    let id_key = match kind {
        "wal" => ("wal_id", RecordOrigin::Wal { wal_id: String::new() }),
        "flush" => ("flush_id", RecordOrigin::Flush { flush_id: String::new() }),
        "sst" => ("sst_id", RecordOrigin::Sst { sst_id: String::new() }),
        other => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/origin"), format!("未知 origin 类型 {other}")));
            return RecordOrigin::Wal { wal_id: String::new() };
        }
    };
    let id = match o.get(id_key.0).and_then(|x| x.as_str()) {
        Some(s) => s.to_string(),
        None => {
            fatal.push(Diagnostic::corrupt(path, format!("origin 缺少 {}", id_key.0)));
            String::new()
        }
    };
    match kind {
        "wal" => RecordOrigin::Wal { wal_id: id },
        "flush" => RecordOrigin::Flush { flush_id: id },
        _ => RecordOrigin::Sst { sst_id: id },
    }
}

fn parse_point(v: &serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<PointRecord> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let key = match o.get("key").and_then(|x| x.as_str()) {
        Some(k) if is_valid_hex(k) => HexKey(k.into()),
        Some(k) => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/key"), format!("key {k} 不是合法偶数长度 hex")));
            HexKey(String::new())
        }
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/key"), "缺少 key"));
            HexKey(String::new())
        }
    };
    let seq = match o.get("seq").and_then(|x| x.as_i64()) {
        Some(n) if n >= 0 => Q::seq(n),
        Some(n) => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/seq"), format!("seq 不能为负: {n}")));
            Q::seq(-1)
        }
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/seq"), "缺少整数 seq"));
            Q::seq(-1)
        }
    };
    let kind = match o.get("kind").and_then(|x| x.as_str()) {
        Some("put") => PointKind::Put,
        Some("delete") => PointKind::Delete,
        Some(other) => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/kind"), format!("point kind 只能是 put/delete: {other}")));
            PointKind::Put
        }
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/kind"), "缺少 kind"));
            PointKind::Put
        }
    };
    let value = o.get("value").map(|rv| match rv.as_object() {
        Some(vo) => RawValue {
            representation: vo.get("representation").and_then(|x| x.as_str()).unwrap_or("utf8").to_string(),
            raw: vo.get("raw").cloned().unwrap_or(serde_json::Value::Null),
        },
        None => RawValue { representation: "utf8".into(), raw: rv.clone() },
    });
    let origin = parse_origin(o.get("origin"), path, fatal);
    Some(PointRecord { id, seq, key, kind, value, origin })
}

fn parse_range(v: &serde_json::Value, path: &str, fatal: &mut Vec<Diagnostic>) -> Option<RangeTombstone> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let start = match o.get("start").and_then(|x| x.as_str()) {
        Some(k) if is_valid_hex(k) => HexKey(k.into()),
        _ => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/start"), "缺少合法 hex start"));
            HexKey(String::new())
        }
    };
    let end = match o.get("end").and_then(|x| x.as_str()) {
        Some(k) if is_valid_hex(k) => HexKey(k.into()),
        _ => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/end"), "缺少合法 hex end"));
            HexKey(String::new())
        }
    };
    if start > end {
        fatal.push(Diagnostic::corrupt(path, "range tombstone start > end（字节序不自洽）"));
    }
    let seq = match o.get("seq").and_then(|x| x.as_i64()) {
        Some(n) if n >= 0 => Q::seq(n),
        _ => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/seq"), "缺少非负整数 seq"));
            Q::seq(-1)
        }
    };
    let end_inclusive = o.get("end_inclusive").and_then(|x| x.as_bool()).unwrap_or(false);
    let origin = parse_origin(o.get("origin"), path, fatal);
    Some(RangeTombstone { id, seq, start, end, end_inclusive, origin })
}

fn parse_wal(v: &serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<WalEntry> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let points = o.get("points").and_then(|x| x.as_array()).map(|a| {
        a.iter().enumerate().flat_map(|(i, p)| parse_point(p, &format!("{path}/points/{i}"), diag, fatal)).collect()
    }).unwrap_or_default();
    let ranges = o.get("ranges").and_then(|x| x.as_array()).map(|a| {
        a.iter().enumerate().flat_map(|(i, r)| parse_range(r, &format!("{path}/ranges/{i}"), fatal)).collect()
    }).unwrap_or_default();
    let wall_time = match o.get("wall_time") {
        Some(vv) => Q::parse_bytes(vv.clone(), &format!("{path}/wall_time"), diag),
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/wall_time"), "缺少 wall_time（时间基准必须显式给出）"));
            Q::from_int(0, "epoch_millis")
        }
    };
    Some(WalEntry { id, points, ranges, wall_time })
}

fn parse_flush(v: &serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<FlushSummary> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let memtable_id = req_str(o, "memtable_id", path, fatal);
    let produced_sst = req_str(o, "produced_sst", path, fatal);
    let wal_entries = match o.get("wal_entries").and_then(|x| x.as_array()) {
        Some(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/wal_entries"), "flush 必须显式列出 wal_entries"));
            vec![]
        }
    };
    let points = o.get("points").and_then(|x| x.as_array()).map(|a| {
        a.iter().enumerate().flat_map(|(i, p)| parse_point(p, &format!("{path}/points/{i}"), diag, fatal)).collect()
    }).unwrap_or_default();
    let ranges = o.get("ranges").and_then(|x| x.as_array()).map(|a| {
        a.iter().enumerate().flat_map(|(i, r)| parse_range(r, &format!("{path}/ranges/{i}"), fatal)).collect()
    }).unwrap_or_default();
    let wall_time = match o.get("wall_time") {
        Some(vv) => Q::parse_bytes(vv.clone(), &format!("{path}/wall_time"), diag),
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/wall_time"), "缺少 wall_time"));
            Q::from_int(0, "epoch_millis")
        }
    };
    Some(FlushSummary { id, memtable_id, wal_entries, produced_sst, points, ranges, wall_time })
}

fn parse_sst(v: &serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<SstSummary> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let level = match o.get("level").and_then(|x| x.as_i64()) {
        Some(n) if n >= 0 => Q::level(n),
        _ => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/level"), "缺少非负整数 level"));
            Q::level(-1)
        }
    };
    let kr = match o.get("key_range") {
        Some(serde_json::Value::Object(kro)) => {
            let smallest = match kro.get("smallest").and_then(|x| x.as_str()) {
                Some(k) if is_valid_hex(k) => HexKey(k.into()),
                _ => {
                    fatal.push(Diagnostic::corrupt(&format!("{path}/key_range/smallest"), "缺少合法 hex smallest"));
                    HexKey(String::new())
                }
            };
            let largest = match kro.get("largest").and_then(|x| x.as_str()) {
                Some(k) if is_valid_hex(k) => HexKey(k.into()),
                _ => {
                    fatal.push(Diagnostic::corrupt(&format!("{path}/key_range/largest"), "缺少合法 hex largest"));
                    HexKey(String::new())
                }
            };
            KeyRange { smallest, largest }
        }
        _ => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/key_range"), "缺少 key_range"));
            KeyRange { smallest: HexKey(String::new()), largest: HexKey(String::new()) }
        }
    };
    let points = o.get("points").and_then(|x| x.as_array()).map(|a| {
        a.iter().enumerate().flat_map(|(i, p)| parse_point(p, &format!("{path}/points/{i}"), diag, fatal)).collect()
    }).unwrap_or_default();
    let ranges = o.get("ranges").and_then(|x| x.as_array()).map(|a| {
        a.iter().enumerate().flat_map(|(i, r)| parse_range(r, &format!("{path}/ranges/{i}"), fatal)).collect()
    }).unwrap_or_default();
    let file_size = match o.get("file_size") {
        Some(vv) => Q::parse_bytes(vv.clone(), &format!("{path}/file_size"), diag),
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/file_size"), "缺少 file_size（不得用默认值伪装）"));
            Q::bytes(-1)
        }
    };
    let provenance = match o.get("provenance") {
        Some(serde_json::Value::Object(po)) => match po.get("kind").and_then(|x| x.as_str()) {
            Some("imported") => SstProvenance::Imported,
            Some("flush") => SstProvenance::Flush {
                flush_id: po.get("flush_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            },
            Some("compaction_output") => SstProvenance::CompactionOutput {
                compaction_id: po.get("compaction_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                output_index: po.get("output_index").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
                input_ssts: po.get("input_ssts").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            },
            other => {
                fatal.push(Diagnostic::corrupt(&format!("{path}/provenance/kind"), format!("未知 provenance {other:?}")));
                SstProvenance::Imported
            }
        },
        None => SstProvenance::Imported,
        Some(_) => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/provenance"), "provenance 必须是对象"));
            SstProvenance::Imported
        }
    };
    Some(SstSummary { id, level, key_range: kr, points, ranges, file_size, provenance })
}

fn parse_params(v: &serde_json::Value, path: &str, diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<CompactionParams> {
    let o = v.as_object()?;
    let version = req_str(o, "version", path, fatal);
    let same_seq_policy = req_str(o, "same_seq_policy", path, fatal);
    let output_bytes_budget = match o.get("output_bytes_budget") {
        Some(vv) => Q::parse_bytes(vv.clone(), &format!("{path}/output_bytes_budget"), diag),
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/output_bytes_budget"), "缺少输出字节预算"));
            Q::bytes(-1)
        }
    };
    let entry_budget = match o.get("entry_budget") {
        Some(serde_json::Value::Number(n)) if n.as_i64().map(|x| x >= 0).unwrap_or(false) => Q::seq(n.as_i64().unwrap()),
        Some(other) => {
            let d = Diagnostic::unconverted(&format!("{path}/entry_budget"), format!("条目预算无法解释: {other}"));
            diag.push(d);
            fatal.push(Diagnostic::corrupt(&format!("{path}/entry_budget"), "entry_budget 必须是非负整数（单位 entry）"));
            Q::seq(-1)
        }
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/entry_budget"), "缺少 entry_budget"));
            Q::seq(-1)
        }
    };
    let max_outputs = match o.get("max_outputs").and_then(|x| x.as_u64()) {
        Some(n) => n,
        None => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/max_outputs"), "缺少 max_outputs"));
            0
        }
    };
    let expand_l0_overlap = o.get("expand_l0_overlap").and_then(|x| x.as_bool()).unwrap_or(true);
    Some(CompactionParams { version, same_seq_policy, output_bytes_budget, entry_budget, max_outputs, expand_l0_overlap })
}

fn parse_seed_decision(v: &serde_json::Value, path: &str, _diag: &mut Vec<Diagnostic>, fatal: &mut Vec<Diagnostic>) -> Option<SeedDecision> {
    let o = v.as_object()?;
    let id = req_str(o, "id", path, fatal);
    let branch = req_str(o, "branch", path, fatal);
    let summary = req_str(o, "summary", path, fatal);
    let decision = match o.get("decision").and_then(|x| x.as_str()) {
        Some(d @ ("accept" | "reject" | "tune")) => d.to_string(),
        other => {
            fatal.push(Diagnostic::corrupt(&format!("{path}/decision"), format!("decision 必须是 accept/reject/tune: {other:?}")));
            String::new()
        }
    };
    let target_compaction = o.get("target_compaction").and_then(|x| x.as_str()).map(String::from);
    let conflicts_with = o.get("conflicts_with").and_then(|x| x.as_str()).map(String::from);
    let note = o.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(SeedDecision { id, branch, summary, decision, target_compaction, conflicts_with, note })
}
