//! 离线案例包「层合迹：LSM tombstone、范围删除与压实选择审阅」。
//! 叙事：存储团队导入 WAL、memtable flush 与多层 SSTable 摘要，
//! 模拟 point/range tombstone 在 compaction 中的保留和下沉。
//! 主分支：基线导入 -> 参数修订 -> 局部源数据改动 -> 冲突人工决定 ->
//!         重叠 L0 / 活跃旧 snapshot / 范围删除端点 / 同序号多类型 /
//!         输出切分 / 压实中断 / 重复安装。
//! 分支 visibility：读取按内部键顺序与 snapshot 判可见性、
//!         范围删除不越过未参与压实的旧数据、输出切分维持不重叠层约束。

use crate::domain::*;
use crate::engine::*;
use crate::store::{EventPayload, Store, StoreError};

pub const BRANCH_MAIN: &str = "main";
pub const BRANCH_VISIBILITY: &str = "visibility";

pub fn load_casepack(store: &Store) -> Result<(), StoreError> {
    // 幂等：已加载则跳过
    if !store.events(Some(BRANCH_MAIN))?.is_empty() {
        return Ok(());
    }
    load_main(store)?;
    load_visibility(store)?;
    Ok(())
}

fn load_main(store: &Store) -> Result<(), StoreError> {
    // ---- 基线：WAL + memtable flush + 多层 SSTable 摘要 ----
    // 同序号多类型：user@7 同时有 PUT 与 DEL（不同来源，seq 相同）
    let mut wal_put = Record::put("user", 7, "v7-wal");
    wal_put.source = "WAL-000441:PUT user@7".into();
    let mut wal_del_same_seq = Record::del("user", 7);
    wal_del_same_seq.source = "WAL-000442:DEL user@7 (crash-recovery 重复序号)".into();

    let l0a = SstFile {
        id: "L0-0007".into(), level: 0,
        smallest: "alpha".into(), largest: "omega".into(),
        records: vec![
            { let mut r = Record::put("alpha", 9, "a9"); r.source = "flush:memtable#3".into(); r },
            { let mut r = Record::put("beta", 8, "b8"); r.source = "flush:memtable#3".into(); r },
            wal_put.clone(),
        ],
        size: Quantity::bytes(96), source: "MANIFEST-000012".into(),
    };
    let l0b = SstFile {
        id: "L0-0008".into(), level: 0,
        smallest: "delta".into(), largest: "user".into(), // 与 L0-0007 重叠
        records: vec![
            { let mut r = Record::del("beta", 10); r.source = "flush:memtable#4".into(); r },
            { let mut r = Record::put("delta", 6, "d6"); r.source = "flush:memtable#4".into(); r },
            wal_del_same_seq.clone(),
        ],
        size: Quantity::bytes(88), source: "MANIFEST-000013".into(),
    };
    let l0c = SstFile {
        id: "L0-0009".into(), level: 0,
        smallest: "gamma".into(), largest: "kappa".into(),
        records: vec![
            // 范围删除端点：[gamma, kappa) 半开区间
            { let mut r = Record::range_del("gamma", "kappa", 12, false); r.source = "flush:memtable#5".into(); r },
            { let mut r = Record::put("gamma", 5, "g5"); r.source = "flush:memtable#5".into(); r },
            { let mut r = Record::put("kappa", 4, "k4"); r.source = "flush:memtable#5".into(); r },
        ],
        size: Quantity::bytes(120), source: "MANIFEST-000014".into(),
    };
    let l0d = SstFile {
        id: "L0-0010".into(), level: 0,
        smallest: "sigma".into(), largest: "zeta".into(),
        records: vec![
            { let mut r = Record::put("sigma", 3, "s3"); r.source = "flush:memtable#6".into(); r },
            { let mut r = Record::put("zeta", 2, "z2"); r.source = "flush:memtable#6".into(); r },
        ],
        size: Quantity::bytes(64), source: "MANIFEST-000015".into(),
    };
    let l1 = SstFile {
        id: "L1-0102".into(), level: 1,
        smallest: "beta".into(), largest: "delta".into(),
        records: vec![
            { let mut r = Record::put("beta", 4, "b4-old"); r.source = "compaction:prev".into(); r },
            { let mut r = Record::put("delta", 3, "d3-old"); r.source = "compaction:prev".into(); r },
        ],
        size: Quantity::bytes(72), source: "MANIFEST-000010".into(),
    };
    let l2 = SstFile {
        id: "L2-0201".into(), level: 2,
        smallest: "gamma".into(), largest: "lambda".into(),
        records: vec![
            { let mut r = Record::put("gamma", 2, "g2-ancient"); r.source = "compaction:prev".into(); r },
            { let mut r = Record::put("lambda", 1, "l1-ancient"); r.source = "compaction:prev".into(); r },
        ],
        size: Quantity::bytes(80), source: "MANIFEST-000008".into(),
    };

    let snapshots = vec![
        Snapshot { id: "snap-old".into(), seq: 5, active: true, label: "活跃旧 snapshot（备份任务持有）".into() },
        Snapshot { id: "snap-new".into(), seq: 11, active: true, label: "当前读快照".into() },
    ];

    let ev_material = store.append(BRANCH_MAIN, EventPayload::MaterialImported {
        files: vec![l0a.clone(), l0b.clone(), l0c.clone(), l0d.clone(), l1.clone(), l2.clone()],
        snapshots: snapshots.clone(),
        source_label: "WAL-000441..442 + memtable flush #3..#6 + MANIFEST 摘要".into(),
    })?;

    // ---- 参数修订 ----
    let mut params = CompactionParams::default();
    params.l0_trigger = 4;
    params.target_file_size = 40; // 调小以触发输出切分
    let ev_params = store.append(BRANCH_MAIN, EventPayload::ParamsRevised {
        params: params.clone(),
        note: "评审决定：target_file_size 2MiB -> 40B 以在案例中演示输出切分".into(),
    })?;

    // ---- 局部源数据改动：只修正 L0-0009 的范围删除终点（kappa -> kappa 含端点）----
    let mut l0c_v2 = l0c.clone();
    l0c_v2.id = "L0-0009".into();
    l0c_v2.records[0] = { let mut r = Record::range_del("gamma", "kappa", 12, true); r.source = "flush:memtable#5 (amended: 终点含 kappa)".into(); r };
    let ev_amend = store.append(BRANCH_MAIN, EventPayload::SourceAmended {
        file: l0c_v2.clone(),
        replaces: "L0-0009".into(),
        note: "WAL 重放发现范围删除终点为闭区间，仅影响 [gamma,kappa] 依赖".into(),
    })?;

    // ---- 压实计算 1：重叠 L0 全量压实（含同序号多类型、范围删除端点、活跃旧 snapshot）----
    let input1 = CompactionInput {
        input_files: vec![l0a.clone(), l0b.clone(), l0c_v2.clone(), l0d.clone(), l1.clone()],
        snapshots: snapshots.clone(),
        params: params.clone(),
        budget: CompactionBudget::default(),
        bottommost_level: false,
        uninvolved_ranges: vec![("gamma".into(), "lambda".into())], // L2 未参与
    };
    let out1 = compact(&input1).map_err(|e| StoreError::Validation(e.to_string()))?;
    let (picked, pick_reason) = pick_compaction(
        &[LevelView { level: 0, files: vec![l0a.clone(), l0b.clone(), l0c_v2.clone(), l0d.clone()] }],
        &params,
    )
    .unwrap_or((vec![], "手动选择".into()));
    let ev_comp1 = store.append(BRANCH_MAIN, EventPayload::CompactionComputed {
        input: input1.clone(),
        output: Some(out1.clone()),
        error: None,
        parent_events: vec![ev_material.id.clone(), ev_params.id.clone(), ev_amend.id.clone()],
        reason: format!("{}；选择文件 {:?}", pick_reason, picked),
    })?;

    // ---- 冲突的人工决定：接受 vs 否定，均保留 ----
    store.append(BRANCH_MAIN, EventPayload::DecisionRecorded {
        action: "accept".into(),
        target_event: ev_comp1.id.clone(),
        rationale: "输出切分维持不重叠层约束，范围删除正确下沉，采纳".into(),
        adopted: true,
    })?;
    store.append(BRANCH_MAIN, EventPayload::DecisionRecorded {
        action: "reject".into(),
        target_event: ev_comp1.id.clone(),
        rationale: "认为 target_file_size=40B 过于激进，要求回退参数（与采纳决定冲突，两条均保留）".into(),
        adopted: false,
    })?;

    // ---- 压实中断：记录预算只够处理 3 条 ----
    let input2 = CompactionInput {
        budget: CompactionBudget { max_output_bytes: u64::MAX, max_records: Some(3) },
        ..input1.clone()
    };
    let out2 = compact(&input2).map_err(|e| StoreError::Validation(e.to_string()))?;
    debug_assert!(out2.interrupted);
    store.append(BRANCH_MAIN, EventPayload::CompactionComputed {
        input: input2.clone(),
        output: Some(out2.clone()),
        error: None,
        parent_events: vec![ev_comp1.id.clone()],
        reason: "资源预算 max_records=3，模拟压实中断".into(),
    })?;
    store.append(BRANCH_MAIN, EventPayload::CompactionInterrupted {
        input_digest: out2.input_digest.clone(),
        processed: 3,
        reason: "记录处理预算耗尽，中断现场已保留".into(),
    })?;

    // ---- 重复安装结果：同一输出安装两次，第二次必须幂等 ----
    let digest = crate::engine::digest_input(&input1);
    store.append(BRANCH_MAIN, EventPayload::ResultInstalled {
        output_digest: digest.clone(), files: out1.files.clone(), install_seq: 1,
    })?;
    store.append(BRANCH_MAIN, EventPayload::ResultInstalled {
        output_digest: digest, files: out1.files.clone(), install_seq: 2,
    })?;

    Ok(())
}

fn load_visibility(store: &Store) -> Result<(), StoreError> {
    // 分支：读取按内部键顺序与 snapshot 判可见性；
    // 范围删除不能越过未参与压实的旧数据而提前丢弃；输出切分维持不重叠层约束。
    let f1 = SstFile {
        id: "V-L0-01".into(), level: 0,
        smallest: "m".into(), largest: "t".into(),
        records: vec![
            Record::range_del("m", "t", 20, true), // [m, t] 闭区间
            Record::put("m", 15, "m15"),
            Record::put("p", 9, "p9"),
            Record::put("t", 21, "t21"),
        ],
        size: Quantity::bytes(64), source: "branch:visibility".into(),
    };
    let f2 = SstFile {
        id: "V-L2-01".into(), level: 2,
        smallest: "n".into(), largest: "s".into(),
        records: vec![Record::put("n", 3, "n3-old"), Record::put("s", 2, "s2-old")],
        size: Quantity::bytes(32), source: "branch:visibility".into(),
    };
    let snaps = vec![Snapshot { id: "snap-vis".into(), seq: 10, active: true, label: "可见性验证快照 seq=10".into() }];
    let ev_m = store.append(BRANCH_VISIBILITY, EventPayload::MaterialImported {
        files: vec![f1.clone(), f2.clone()],
        snapshots: snaps.clone(),
        source_label: "可见性分支基线".into(),
    })?;

    let params = CompactionParams { version: 1, l0_trigger: 2, target_file_size: 30, allow_range_del_elision: true };
    store.append(BRANCH_VISIBILITY, EventPayload::ParamsRevised { params: params.clone(), note: "分支参数".into() })?;

    // 压实到底层，但 L2 的 [n,s] 未参与：范围删除 [m,t] 与之重叠，必须保留
    let input = CompactionInput {
        input_files: vec![f1.clone()],
        snapshots: snaps,
        params: params.clone(),
        budget: CompactionBudget::default(),
        bottommost_level: true,
        uninvolved_ranges: vec![("n".into(), "s".into())],
    };
    let out = compact(&input).map_err(|e| StoreError::Validation(e.to_string()))?;
    store.append(BRANCH_VISIBILITY, EventPayload::CompactionComputed {
        input,
        output: Some(out),
        error: None,
        parent_events: vec![ev_m.id],
        reason: "范围删除 [m,t] 与未参与压实的 L2 [n,s] 重叠，虽到底层仍不得提前丢弃".into(),
    })?;
    Ok(())
}
