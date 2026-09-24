//! 验收测试：重叠 L0、活跃旧 snapshot、范围删除端点、同序号多类型、
//! 输出切分、压实中断、重复安装、导出导入往返、半写恢复、三类错误区分。

use lsm_review::casepack;
use lsm_review::domain::*;
use lsm_review::engine::*;
use lsm_review::projection::project_store;
use lsm_review::store::{EventPayload, Store};

fn f(id: &str, level: u32, lo: &str, hi: &str, recs: Vec<Record>) -> SstFile {
    SstFile { id: id.into(), level, smallest: lo.into(), largest: hi.into(), records: recs, size: Quantity::bytes(10), source: "test".into() }
}

fn base_input(files: Vec<SstFile>) -> CompactionInput {
    CompactionInput {
        input_files: files,
        snapshots: vec![],
        params: CompactionParams::default(),
        budget: CompactionBudget::default(),
        bottommost_level: false,
        uninvolved_ranges: vec![],
    }
}

#[test]
fn overlapping_l0_is_allowed_and_merged() {
    // L0 允许重叠；非 L0 重叠则是语义无解
    let a = f("L0-a", 0, "a", "m", vec![Record::put("a", 5, "1")]);
    let b = f("L0-b", 0, "k", "z", vec![Record::put("z", 4, "2")]);
    let out = compact(&base_input(vec![a, b])).expect("L0 重叠应被允许");
    assert!(!out.files.is_empty());

    let c = f("L1-a", 1, "a", "m", vec![Record::put("a", 5, "1")]);
    let d = f("L1-b", 1, "k", "z", vec![Record::put("z", 4, "2")]);
    let err = compact(&base_input(vec![c, d])).unwrap_err();
    assert!(matches!(err, EngineError::SemanticConflict(_)), "非 L0 重叠必须是语义无解");
}

#[test]
fn active_old_snapshot_retains_old_versions() {
    let file = f("L0-1", 0, "k", "k", vec![Record::put("k", 9, "new"), Record::put("k", 3, "old")]);
    let mut input = base_input(vec![file]);
    // 无快照：旧版本被丢弃
    let out = compact(&input).unwrap();
    let kept: Vec<_> = out.entries.iter().filter(|e| e.decision == EntryDecision::Kept).collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].record.ikey.seq, 9);
    // 活跃旧 snapshot seq=5：旧版本必须保留
    input.snapshots = vec![Snapshot { id: "s".into(), seq: 5, active: true, label: String::new() }];
    let out = compact(&input).unwrap();
    let kept: Vec<_> = out.entries.iter().filter(|e| e.decision == EntryDecision::Kept).collect();
    assert_eq!(kept.len(), 2, "活跃旧 snapshot 需要旧版本");
    assert!(kept.iter().any(|e| e.reason.contains("snapshot")));
}

#[test]
fn range_delete_endpoint_semantics() {
    // 半开 [a, c)：不覆盖 c；闭区间 [a, c]：覆盖 c
    let half_open = f("L0-1", 0, "a", "c", vec![Record::range_del("a", "c", 10, false), Record::put("c", 5, "cv")]);
    let out = compact(&base_input(vec![half_open])).unwrap();
    let c_entry = out.entries.iter().find(|e| e.record.ikey.user_key == "c").unwrap();
    assert_eq!(c_entry.decision, EntryDecision::Kept, "半开区间终点不得被范围删除覆盖");

    let closed = f("L0-1", 0, "a", "c", vec![Record::range_del("a", "c", 10, true), Record::put("c", 5, "cv")]);
    let out = compact(&base_input(vec![closed])).unwrap();
    let c_entry = out.entries.iter().find(|e| e.record.ikey.user_key == "c").unwrap();
    assert_eq!(c_entry.decision, EntryDecision::DroppedShadowed, "闭区间终点必须被覆盖");
}

#[test]
fn same_seq_multiple_kinds_ordering() {
    // 同 (key, seq) 的 PUT 与 DEL：内部键序 PUT 在 DEL 前，两者都按各自语义处理
    let file = f("L0-1", 0, "k", "k", vec![Record::del("k", 7), Record::put("k", 7, "v7")]);
    let out = compact(&base_input(vec![file])).unwrap();
    let kinds: Vec<u8> = out.entries.iter().map(|e| e.record.ikey.kind).collect();
    assert_eq!(kinds, vec![KIND_POINT, KIND_DELETE], "同序号时 PUT 必须先于 DEL");
}

#[test]
fn output_split_preserves_non_overlapping_constraint() {
    let recs: Vec<Record> = (0..20).map(|i| Record::put(&format!("k{:02}", i), 5, "value-padding")).collect();
    let file = f("L0-1", 0, "k00", "k19", recs);
    let mut input = base_input(vec![file]);
    input.params.target_file_size = 60;
    let out = compact(&input).unwrap();
    assert!(out.files.len() > 1, "应触发输出切分");
    for w in out.files.windows(2) {
        assert!(w[0].largest < w[1].smallest, "切分后同层文件不得重叠: {} vs {}", w[0].id, w[1].id);
    }
    // 同一 user_key 的版本不得被切到两个文件
    for w in out.files.windows(2) {
        assert_ne!(w[0].largest, w[1].smallest);
    }
}

#[test]
fn compaction_interruption_preserves_scene() {
    let recs: Vec<Record> = (0..10).map(|i| Record::put(&format!("k{}", i), 5, "v")).collect();
    let file = f("L0-1", 0, "k0", "k9", recs);
    let mut input = base_input(vec![file]);
    input.budget.max_records = Some(4);
    let out = compact(&input).unwrap();
    assert!(out.interrupted, "预算耗尽必须标记中断");
    assert_eq!(out.entries.len(), 4, "中断前处理的记录数应保留");
}

#[test]
fn duplicate_install_is_idempotent() {
    let store = Store::open_memory().unwrap();
    let files = vec![f("out-1", 1, "a", "b", vec![Record::put("a", 1, "x")])];
    store.append("main", EventPayload::ResultInstalled { output_digest: "d".into(), files: files.clone(), install_seq: 1 }).unwrap();
    store.append("main", EventPayload::ResultInstalled { output_digest: "d".into(), files: files.clone(), install_seq: 2 }).unwrap();
    let st = project_store(&store, Some("main"), None).unwrap();
    let l1 = st.levels.iter().find(|l| l.level == 1).unwrap();
    assert_eq!(l1.files.len(), 1, "重复安装不得产生重复文件");
}

#[test]
fn range_delete_not_dropped_past_uninvolved_data() {
    let rd = f("L0-1", 0, "m", "t", vec![Record::range_del("m", "t", 20, true)]);
    let mut input = base_input(vec![rd]);
    input.bottommost_level = true;
    input.uninvolved_ranges = vec![("n".into(), "s".into())];
    let out = compact(&input).unwrap();
    let entry = &out.entries[0];
    assert_eq!(entry.decision, EntryDecision::Kept, "范围删除不得越过未参与压实的旧数据提前丢弃");
    assert!(entry.reason.contains("未参与压实"));

    // 无重叠未参与数据时，底层可丢弃
    input.uninvolved_ranges = vec![];
    let out = compact(&input).unwrap();
    assert_eq!(out.entries[0].decision, EntryDecision::DroppedRangeCovered);
}

#[test]
fn equivalent_inputs_give_stable_order() {
    let a = f("L0-b", 0, "k", "z", vec![Record::put("z", 4, "2"), Record::put("k", 5, "1")]);
    let b = f("L0-a", 0, "a", "j", vec![Record::put("a", 3, "0")]);
    let out1 = compact(&base_input(vec![a.clone(), b.clone()])).unwrap();
    let out2 = compact(&base_input(vec![b, a])).unwrap(); // 输入顺序不同
    let json1 = serde_json::to_string(&out1.entries).unwrap();
    let json2 = serde_json::to_string(&out2.entries).unwrap();
    assert_eq!(json1, json2, "等价输入必须得到稳定顺序");
}

#[test]
fn three_error_kinds_are_distinct() {
    // 预算耗尽
    let file = f("L0-1", 0, "a", "z", (0..50).map(|i| Record::put(&format!("k{:02}", i), 5, "vvvvvvvv")).collect());
    let mut input = base_input(vec![file]);
    input.budget.max_output_bytes = 1;
    let err = compact(&input).unwrap_err();
    assert!(matches!(err, EngineError::BudgetExhausted(_)));

    // 格式损坏
    let mut bad = f("L0-1", 0, "z", "a", vec![]); // range 倒置
    bad.records = vec![];
    let err = compact(&base_input(vec![bad])).unwrap_err();
    assert!(matches!(err, EngineError::CorruptFormat(_)));

    // 语义无解
    let c = f("L1-a", 1, "a", "m", vec![Record::put("a", 5, "1")]);
    let d = f("L1-b", 1, "k", "z", vec![Record::put("z", 4, "2")]);
    let err = compact(&base_input(vec![c, d])).unwrap_err();
    assert!(matches!(err, EngineError::SemanticConflict(_)));
}

#[test]
fn export_import_roundtrip_preserves_identity_and_order() {
    let store = Store::open_memory().unwrap();
    casepack::load_casepack(&store).unwrap();
    let exported = store.export().unwrap();
    let before = store.events(None).unwrap();

    // 删除本地数据库再导入
    let store2 = Store::open_memory().unwrap();
    let n = store2.import(&exported).unwrap();
    assert_eq!(n, before.len());
    let after = store2.events(None).unwrap();
    let ids_before: Vec<_> = before.iter().map(|e| (e.branch.clone(), e.id.clone())).collect();
    let ids_after: Vec<_> = after.iter().map(|e| (e.branch.clone(), e.id.clone())).collect();
    assert_eq!(ids_before, ids_after, "身份、顺序、分支必须保持");

    // 重复导入幂等
    let n2 = store2.import(&exported).unwrap();
    assert_eq!(store2.events(None).unwrap().len(), before.len(), "重复导入不得翻倍（{} 条重新提交）", n2);
}

#[test]
fn import_rejects_corrupt_checksum_with_positioned_diagnostic() {
    let store = Store::open_memory().unwrap();
    casepack::load_casepack(&store).unwrap();
    let exported = store.export().unwrap();
    let tampered = exported.replacen("\"seq\":5", "\"seq\":6", 1);
    let store2 = Store::open_memory().unwrap();
    let err = store2.import(&tampered).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("checksum") || msg.contains("校验"), "损坏必须被诊断而非静默成功: {}", msg);
}

#[test]
fn half_written_import_staging_is_recovered() {
    let dir = std::env::temp_dir().join(format!("lsm-halfwrite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("t.db");
    {
        let store = Store::open(&db).unwrap();
        // 模拟导入中断留下的暂存数据
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("INSERT INTO import_staging (chunk) VALUES (x'deadbeef')", []).unwrap();
        }
    }
    // 重新打开：暂存区必须被清理（半写恢复）
    let store = Store::open(&db).unwrap();
    {
        let conn = store.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM import_staging", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "半写暂存必须在打开时恢复清理");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn casepack_loads_and_projects_all_scenarios() {
    let store = Store::open_memory().unwrap();
    casepack::load_casepack(&store).unwrap();
    // 幂等
    casepack::load_casepack(&store).unwrap();
    let st = project_store(&store, Some(casepack::BRANCH_MAIN), None).unwrap();
    assert!(st.levels.iter().any(|l| l.level == 0), "应有 L0 层");
    assert!(!st.compactions.is_empty(), "应有压实记录");
    assert!(st.compactions.iter().any(|c| c.output.as_ref().map_or(false, |o| o.interrupted)), "应有中断的压实");
    assert!(st.decisions.iter().any(|d| d.adopted) && st.decisions.iter().any(|d| !d.adopted), "采纳与未采纳决定均保留");
    assert!(!st.interruptions.is_empty(), "应有中断记录");
    assert!(!st.param_history.is_empty(), "应有参数修订历史");
    assert!(!st.tombstone_coverage.is_empty(), "应有 tombstone 覆盖");
    // 同序号多类型
    let chain = st.version_chains.get("user").expect("user 版本链");
    assert!(chain.iter().any(|r| r.ikey.kind == KIND_POINT && r.ikey.seq == 7));
    assert!(chain.iter().any(|r| r.ikey.kind == KIND_DELETE && r.ikey.seq == 7));

    // 可见性分支
    let vst = project_store(&store, Some(casepack::BRANCH_VISIBILITY), None).unwrap();
    let comp = vst.compactions.first().expect("可见性分支压实");
    let out = comp.output.as_ref().unwrap();
    let rdel = out.entries.iter().find(|e| e.record.is_range_del()).unwrap();
    assert_eq!(rdel.decision, EntryDecision::Kept, "范围删除不得越过未参与压实的旧数据");
}

#[test]
fn time_travel_replay_restores_historical_state() {
    let store = Store::open_memory().unwrap();
    casepack::load_casepack(&store).unwrap();
    let events = store.events(Some(casepack::BRANCH_MAIN)).unwrap();
    let first_seq = events.first().unwrap().seq;
    let st_early = project_store(&store, Some(casepack::BRANCH_MAIN), Some(first_seq)).unwrap();
    let st_now = project_store(&store, Some(casepack::BRANCH_MAIN), None).unwrap();
    assert!(st_early.compactions.len() < st_now.compactions.len(), "回到旧事件应恢复当时状态");
    assert_eq!(st_early.replayed_until, first_seq);
}

#[test]
fn visibility_query_follows_internal_key_order_and_snapshot() {
    let files = vec![f("L0-1", 0, "a", "c", vec![
        Record::put("a", 9, "a9"),
        Record::put("a", 4, "a4"),
        Record::del("b", 8),
        Record::put("b", 3, "b3"),
        Record::put("c", 2, "c2"),
    ])];
    let vis10 = visible_at(&files, 10);
    assert_eq!(vis10, vec![
        ("a".to_string(), Some("a9".to_string())),
        ("b".to_string(), None),
        ("c".to_string(), Some("c2".to_string())),
    ]);
    let vis5 = visible_at(&files, 5);
    assert_eq!(vis5, vec![
        ("a".to_string(), Some("a4".to_string())),
        ("b".to_string(), Some("b3".to_string())),
        ("c".to_string(), Some("c2".to_string())),
    ], "旧快照应看到旧版本");
}
