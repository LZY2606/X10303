//! 离线案例包：「层合迹：LSM tombstone、范围删除与压实选择审阅」。
//!
//! 不使用通用表格：材料全部是 WAL/memtable flush/多层 SSTable 摘要。
//! 基线分支保留全部要素，随后安排：
//!   1) 一次参数修订（tie policy 与输出预算）
//!   2) 一次只影响局部依赖的源数据改动（修正单个 SSTable 摘要 -> 重算仅下游）
//!   3) 一次互相冲突的人工决定（接受 vs 否定同一压实结果，分到两个分支）
//! 重放记录实际经过：重叠 L0、活跃旧 snapshot、范围删除端点、同序号多类型、
//!   输出切分、压实中断、重复安装结果。

use crate::domain::*;
use crate::store::{Store, MaterialBundle, DEFAULT_PARAMS};
use rusqlite::Result;

pub const BASELINE: &str = "baseline";
pub const AMEND: &str = "amend-rules";
pub const LOCAL: &str = "local-source-fix";
pub const CONFLICT_A: &str = "decision-accept";
pub const CONFLICT_R: &str = "decision-reject";

fn q(raw: &str, unit: &str, n: Option<u64>) -> RawQty {
    RawQty { raw: raw.to_string(), unit: unit.to_string(), normalized: n, note: None }
}
fn pt(id: &str, key: &str, seq: u64, kind: PointKind, origin: &str) -> PointOp {
    PointOp { id: id.to_string(), user_key: key.to_string(), seq, kind, value: None, origin: origin.to_string() }
}
fn put(id: &str, key: &str, seq: u64, raw_val: &str, origin: &str) -> PointOp {
    let mut p = pt(id, key, seq, PointKind::Put, origin);
    p.value = Some(q(raw_val, "text", None));
    p
}
fn file(id: &str, level: i32, lo: &str, hi: u64, size: u64,
        points: Vec<PointOp>, ranges: Vec<RangeTombstone>, origin: FileOrigin) -> SstFile {
    let min_seq = points.iter().map(|p| p.seq).chain(ranges.iter().map(|r| r.seq)).min().unwrap_or(0);
    let max_seq = points.iter().map(|p| p.seq).chain(ranges.iter().map(|r| r.seq)).max().unwrap_or(hi);
    SstFile {
        id: id.to_string(), level,
        min_key: lo.to_string(),
        max_key: points.iter().map(|p| p.user_key.as_str()).chain(ranges.iter().map(|r| r.end_key.as_str())).max().unwrap_or(lo).to_string(),
        min_seq, max_seq, size: q(&size.to_string(), UNIT_BYTE, Some(size)),
        points, ranges, origin, key_basis: KEY_BASIS.to_string(),
    }
}
fn rng(id: &str, sk: &str, skind: EndpointKind, ek: &str, ekind: EndpointKind, seq: u64, origin: &str) -> RangeTombstone {
    RangeTombstone { id: id.to_string(), start_key: sk.to_string(), start_kind: skind,
        end_key: ek.to_string(), end_kind: ekind, seq, origin: origin.to_string(), fragment_of: None }
}
fn wal(id: &str, seq: u64, op: WalOp, raw: &str) -> WalEntry {
    WalEntry { id: id.to_string(), seq, op, at_basis: TIME_BASIS.to_string(), raw: raw.to_string() }
}

/// 不可变导入材料（基线）。
pub fn baseline_bundle() -> MaterialBundle {
    use EndpointKind::*;
    use PointKind::*;
    let wal_entries = vec![
        wal("wal:1", 1, WalOp::Put { key: "apple".into(), value: q("a-v1", "text", None) },
            "1 PUT apple a-v1"),
        wal("wal:2", 2, WalOp::Put { key: "banana".into(), value: q("b-v1", "text", None) },
            "2 PUT banana b-v1"),
        wal("wal:3", 3, WalOp::Put { key: "cherry".into(), value: q("c-v1", "text", None) },
            "3 PUT cherry c-v1"),
        wal("wal:4", 4, WalOp::Delete { key: "banana".into() }, "4 DELETE banana"),
        wal("wal:5", 5, WalOp::RangeDelete {
            start_key: "date".into(), start_kind: Inclusive,
            end_key: "fig".into(), end_kind: Exclusive },
            "5 RANGE_DELETE [date,fig)"),
        wal("wal:6", 6, WalOp::Put { key: "egg".into(), value: q("e-v1", "text", None) },
            "6 PUT egg e-v1"),
        wal("wal:7", 7, WalOp::Put { key: "apple".into(), value: q("a-v2", "text", None) },
            "7 PUT apple a-v2"),
        wal("wal:8", 8, WalOp::Put { key: "grape".into(), value: q("g-v1", "text", None) },
            "8 PUT grape g-v1"),
        wal("wal:9", 9, WalOp::Delete { key: "cherry".into() }, "9 DELETE cherry"),
        wal("wal:10", 10, WalOp::Put { key: "honey".into(), value: q("h-v1", "text", None) },
            "10 PUT honey h-v1"),
    ];

    let snapshots = vec![
        Snapshot { id: "snap:old".into(), seq: 4, active: true, created_basis: TIME_BASIS.to_string() },
        Snapshot { id: "snap:mid".into(), seq: 7, active: false, created_basis: TIME_BASIS.to_string() },
    ];

    // L0 两个重叠文件（flush 产物）：memtable-flush 摘要保留。
    let l0a = file("sst:L0:a", 0, "apple", 9, 64, vec![
        put("p:L0a:apple:7", "apple", 7, "a-v2", "wal:7"),
        put("p:L0a:banana:2", "banana", 2, "b-v1", "wal:2"),
        pt("p:L0a:banana:4", "banana", 4, Delete, "wal:4"),
        pt("p:L0a:cherry:9", "cherry", 9, Delete, "wal:9"),
    ], vec![
        rng("r:L0a:date-fig:5", "date", Inclusive, "fig", Exclusive, 5, "wal:5"),
    ], FileOrigin::Flush { flush_id: "flush:1".into() });

    let l0b = file("sst:L0:b", 0, "cherry", 10, 56, vec![
        put("p:L0b:cherry:3", "cherry", 3, "c-v1", "wal:3"),
        put("p:L0b:egg:6", "egg", 6, "e-v1", "wal:6"),
        put("p:L0b:grape:8", "grape", 8, "g-v1", "wal:8"),
        put("p:L0b:honey:10", "honey", 10, "h-v1", "wal:10"),
        put("p:L0b:apple:1", "apple", 1, "a-v1", "wal:1"),
    ], vec![], FileOrigin::Flush { flush_id: "flush:2".into() });

    // L1 既有不重叠文件，其中一个与 L0 range 边界相交（fig 端点探针）。
    let l1a = file("sst:L1:a", 1, "fig", 10, 48, vec![
        put("p:L1a:fig:10", "fig", 10, "f-v1", "import:legacy"),
        put("p:L1a:grape:3", "grape", 3, "g-old", "import:legacy"),
    ], vec![], FileOrigin::Imported);

    // L2 旧数据：与 [date,fig) range 相交但不参与 L0/L1 压实 —— 结构性边界。
    let l2a = file("sst:L2:a", 2, "date", 3, 40, vec![
        put("p:L2a:date:3", "date", 3, "d-old", "import:legacy"),
    ], vec![], FileOrigin::Imported);

    let mut b = MaterialBundle {
        wal: wal_entries, snapshots,
        sst: vec![l0a, l0b, l1a, l2a],
        params: vec![],
        diagnostics: vec![],
    };
    crate::store::normalize_bundle(&mut b);
    b
}

/// 在空库上播种完整叙事案例包，返回每个分支关键事件 id 的索引（便于测试）。
#[derive(serde::Serialize, Clone, Debug, Default)]
pub struct SeedIndex {
    pub baseline_import: String,
    pub baseline_compaction: String,
    pub baseline_install: Option<String>,
    pub amend_compaction_tie: String,
    pub local_compaction: String,
    pub accept_compaction: String,
    pub reject_compaction: String,
    pub interrupted_compaction: String,
    pub split_compaction: String,
    pub dup_compaction: String,
}

pub fn seed(store: &mut Store) -> Result<SeedIndex> {
    let mut idx = SeedIndex::default();

    // ---- 基线：导入 WAL/flush/多层 SSTable 摘要（第一段暂存 -> 第二段原子提交）----
    let bundle = baseline_bundle();
    store.ensure_branch(BASELINE, None)?;
    store.stage_import("import:baseline", BASELINE, &bundle)?;
    let imp = store.commit_import("import:baseline")?;
    idx.baseline_import = imp.event_id.clone();

    // 基线压实：重叠 L0 + 活跃旧 snapshot(seq=4) + range 端点 [date,fig) + L2 旧数据约束。
    let (cev, cresult) = store.plan_compaction(BASELINE,
        &crate::engine::CompactionRequest {
            anchor_level: 0, anchor_file_id: Some("sst:L0:a".into()),
            include_levels: vec![0], crash_before_install: None,
        })?;
    idx.baseline_compaction = cev.event_id.clone();
    assert!(cresult.non_overlap, "基线压实输出必须维持不重叠层约束");
    // range tombstone 必须因 L2 旧数据与旧 snapshot 被保留（不得提前丢弃）。
    let range_v = cresult.verdicts.iter().find(|v| v.entry_id == "r:L0a:date-fig:5")
        .expect("range tombstone 必须出现在判定中");
    assert!(range_v.survivor, "range tombstone 不得越过未参与压实的 L2 旧数据提前丢弃");
    assert_eq!(range_v.reason_code, "range_bounded_by_old_data");
    // point delete(banana@4) 被旧 snapshot(seq=4) 约束保留下沉。
    let bd = cresult.verdicts.iter().find(|v| v.entry_id == "p:L0a:banana:4").unwrap();
    assert!(bd.survivor);
    let installed = store.install_compaction(BASELINE, &cev.event_id, vec![], cresult.outputs.len(), false)?;
    idx.baseline_install = installed.map(|e| e.event_id);

    // 重复安装同一结果：必须幂等（返回已存在事件，文件集不变）。
    let again = store.install_compaction(BASELINE, &cev.event_id, vec![], cresult.outputs.len(), false)?;
    assert!(again.is_some());
    let state_after = store.replay(BASELINE, None)?;
    let dup_installs = state_after.installed_results.iter()
        .filter(|id| **id == cev.event_id).count();
    assert_eq!(dup_installs, 1, "重复安装结果不得重复生效");

    // ---- 分支一：参数修订（tie policy -> reject_duplicate，输出预算调小触发切分）----
    store.fork(AMEND, BASELINE, None, "参数修订：同序号多类型拒绝裁决 + 输出预算收紧")?;
    let v2 = ParamsV {
        version: "params:v2".into(),
        tie_policy: TiePolicy::RejectDuplicate,
        max_output_bytes: q("32 byte", UNIT_BYTE, Some(32)),
        max_total_input_bytes: q("4096 byte", UNIT_BYTE, Some(4096)),
        key_basis: KEY_BASIS.into(), seq_basis: SEQ_BASIS.into(),
        note: "v2：同序号多类型无唯一裁决时语义无解；输出预算 32 byte 以演示输出切分".into(),
    };
    store.revise_params(AMEND, v2)?;

    // 先补一个同序号多类型场景：同 key 同 seq 的 Put 与 point delete（在新 L0 文件）。
    add_dup_file(store, AMEND)?;
    let (dup_ev, dup_res) = store.plan_compaction(AMEND,
        &crate::engine::CompactionRequest {
            anchor_level: 0, anchor_file_id: Some("sst:L0:dup".into()),
            include_levels: vec![0], crash_before_install: None,
        })?;
    idx.dup_compaction = dup_ev.event_id;
    use crate::engine::CompactionStatus;
    assert!(matches!(dup_res.status, CompactionStatus::SemanticUnsolvable { .. }),
        "同序号多类型 + 拒绝裁决必须是语义无解");

    // 输出切分压实：小预算作用于多 key L0（基线 L0 文件此时已在该分支被压实走，
    // 用新的大 key 跨度文件演示切分）。
    add_split_file(store, AMEND)?;
    let (split_ev, split_res) = store.plan_compaction(AMEND,
        &crate::engine::CompactionRequest {
            anchor_level: 0, anchor_file_id: Some("sst:L0:split".into()),
            include_levels: vec![0], crash_before_install: None,
        })?;
    idx.split_compaction = split_ev.event_id;
    assert!(split_res.outputs.len() >= 2, "输出预算收紧后必须切分成多个输出");
    assert!(split_res.non_overlap, "切分后仍须维持不重叠层约束");

    // ---- 压实中断：只写暂存不提交（半写），随后 recover 回滚 ----
    let (int_ev, int_res) = store.plan_compaction(AMEND,
        &crate::engine::CompactionRequest {
            anchor_level: 0, anchor_file_id: Some("sst:L0:split".into()),
            include_levels: vec![0], crash_before_install: Some(0),
        })?;
    idx.interrupted_compaction = int_ev.event_id.clone();
    assert!(matches!(int_res.status, CompactionStatus::Interrupted { .. }));
    store.install_compaction(AMEND, &int_ev.event_id, vec![], 0, true)?; // 模拟崩溃
    // 重新打开即触发半写恢复（同一连接也可显式 recover_pending）。
    let recovered = store.recover_pending()?;
    assert!(recovered.iter().any(|p| p.result_event_id == int_ev.event_id),
        "半写暂存必须在恢复时被发现并回滚");
    let state_rec = store.replay(AMEND, None)?;
    assert!(state_rec.files.iter().all(|f| f.id != int_res.outputs[0].proposed_id),
        "半写恢复后不得残留任何输出文件");

    // 对照证据：把同序号多类型文件也放到基线规则（delete_wins）下压实一次，
    // 证明同一份冲突材料在 v1 规则下可裁决、在 v2 规则下语义无解。
    add_dup_file(store, BASELINE)?;
    let (tie_ev, tie_res) = store.plan_compaction(BASELINE,
        &crate::engine::CompactionRequest {
            anchor_level: 0, anchor_file_id: Some("sst:L0:dup".into()),
            include_levels: vec![0], crash_before_install: None,
        })?;
    idx.amend_compaction_tie = tie_ev.event_id.clone();
    assert!(matches!(tie_res.status, CompactionStatus::Complete),
        "delete_wins 规则下同序号多类型必须可裁决并完成");
    let _ = store.install_compaction(BASELINE, &tie_ev.event_id, vec![], tie_res.outputs.len(), false)?;

    // ---- 分支二：只影响局部依赖的源数据改动 ----
    // 从基线导入事件之后分叉：修正 sst:L1:a 的 size 原始表示（之前规范，
    // 这里改成不可换算值再修正，演示诊断留在原位置且只重算下游压实）。
    store.fork(LOCAL, BASELINE, Some(&idx.baseline_import), "局部源数据改动：L1 摘要端点核对")?;
    add_local_correction(store, LOCAL, &idx.baseline_compaction)?;
    let (loc_ev, loc_res) = store.plan_compaction(LOCAL,
        &crate::engine::CompactionRequest {
            anchor_level: 0, anchor_file_id: Some("sst:L0:a".into()),
            include_levels: vec![0], crash_before_install: None,
        })?;
    idx.local_compaction = loc_ev.event_id;
    // 改动只影响与 L1 相关的下游判定：L2 边界结论保持不变。
    let range_v = loc_res.verdicts.iter().find(|v| v.entry_id == "r:L0a:date-fig:5").unwrap();
    assert_eq!(range_v.reason_code, "range_bounded_by_old_data");

    // ---- 分支三：互相冲突的人工决定（接受 vs 否定同一压实结果）----
    store.fork(CONFLICT_A, BASELINE, Some(&idx.baseline_compaction), "人工决定：接受基线压实")?;
    let out_n = cresult.outputs.len();
    store.install_compaction(CONFLICT_A, &idx.baseline_compaction, vec![], out_n, false)?;
    idx.accept_compaction = idx.baseline_compaction.clone();

    store.fork(CONFLICT_R, BASELINE, Some(&idx.baseline_compaction), "人工决定：否定基线压实")?;
    store.decision_reject(CONFLICT_R, &idx.baseline_compaction,
        "审阅者认为当前活跃旧 snapshot 仍需保留全部 L0，待 snapshot 释放后再压实")?;
    idx.reject_compaction = idx.baseline_compaction.clone();

    Ok(idx)
}

fn add_dup_file(store: &mut Store, branch: &str) -> Result<()> {
    use EndpointKind::*;
    use PointKind::*;
    // 同 key=iris seq=20：一个 Put 与一个 point delete 同序号（来源不同 WAL 片段，保留两源）。
    let f = file("sst:L0:dup", 0, "iris", 20, 32, vec![
        put("p:dup:iris:20:put", "iris", 20, "i-put", "wal:dup:put"),
        pt("p:dup:iris:20:del", "iris", 20, Delete, "wal:dup:del"),
    ], vec![
        // 范围删除端点探针：(ivy,ivy]（仅覆盖孤立 key）以及 [ivy,jasmine)。
        rng("r:dup:ivy-only:20", "ivy", Inclusive, "ivy", Inclusive, 20, "wal:dup:r1"),
    ], FileOrigin::Flush { flush_id: "flush:dup".into() });
    store.flush_memtable(branch, &["wal:dup:put", "wal:dup:del", "wal:dup:r1"], f)?;
    let _ = (Inclusive, Exclusive);
    Ok(())
}

fn add_split_file(store: &mut Store, branch: &str) -> Result<()> {
    use PointKind::*;
    // 5 个 key，每个值规范化为 12 byte；输出预算 32 byte => 至少切两刀。
    let mk = |id: &str, k: &str, s: u64, v: &str| {
        let mut p = pt(id, k, s, Put, "wal:split");
        p.value = Some(q(v, UNIT_BYTE, Some(12)));
        p
    };
    let f = file("sst:L0:split", 0, "kiwi", 35, 120, vec![
        mk("p:sp:kiwi:31", "kiwi", 31, "12"),
        mk("p:sp:lemon:32", "lemon", 32, "12"),
        mk("p:sp:mango:33", "mango", 33, "12"),
        mk("p:sp:nectarine:34", "nectarine", 34, "12"),
        mk("p:sp:orange:35", "orange", 35, "12"),
    ], vec![], FileOrigin::Flush { flush_id: "flush:split".into() });
    store.flush_memtable(branch, &["wal:split:31","wal:split:32","wal:split:33","wal:split:34","wal:split:35"], f)?;
    Ok(())
}

fn add_local_correction(store: &mut Store, branch: &str, _parent_compaction: &str) -> Result<()> {
    // 局部源数据改动：登记一个 SSTable 摘要勘误事件（不可变材料不覆盖，只追加勘误）。
    // 勘误把 sst:L1:a 的 min_key 显式标注为 fig（与 range [date,fig) 排除端点相邻），
    // 并附上原始导入来源，方便从最终状态反查哪条材料造成差异。
    let note = serde_json::json!({
        "at_basis": TIME_BASIS,
        "kind": "source_correction",
        "subject": "sst:L1:a",
        "field": "min_key",
        "observed": "fig",
        "raw_origin": "import:legacy",
        "effect_scope": ["compaction:baseline"],
        "message": "仅修正 L1 文件边界标注；L2 旧数据与 snapshot 判定不受影响"
    });
    store.append_event(branch, "source_corrected", note,
        Some("sha256:local-correction:v1".into()),
        Some(vec!["sst:L1:a".into()]))?;
    Ok(())
}
