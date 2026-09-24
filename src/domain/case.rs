//! 离线案例包：层合迹——LSM tombstone、范围删除与压实选择审阅。
//!
//! 不使用通用表格：案例材料就是 WAL / memtable flush / 多层 SSTable 摘要本身，
//! 并按存储团队导入的完整叙事串联。参数修订、局部源数据改动、互相冲突的人工决定
//! 都安排在同一条证据线上。

use crate::domain::engine::{Command, DecideAction, Engine, ProbeSpec};
use crate::domain::model::*;
use crate::domain::units::{Basis, Measured, Quantity};
use serde::{Deserialize, Serialize};

pub const MAIN_BRANCH: &str = "main";
pub const READ_BRANCH: &str = "read-semantics";
pub const CASE_FORMAT: &str = "lsm-audit/case-pack";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseStep {
    pub ordinal: u32,
    pub title: String,
    pub branch: String,
    pub epoch_ms: u64,
    pub command: Command,
    pub expect: Vec<String>,
    pub expect_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantEvent {
    pub id: String,
    pub rule: String,
    pub statement: String,
    pub witnesses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CasePack {
    pub format: String,
    pub title: String,
    pub description: String,
    pub basis: Basis,
    pub initial_params: Params,
    pub steps: Vec<CaseStep>,
    pub invariants: Vec<InvariantEvent>,
}

fn measured(value: serde_json::Value, unit: &str, quantity: Quantity) -> Option<Measured> {
    Some(Measured::new(value, unit, quantity))
}

fn celsius(raw: f64) -> Option<Measured> {
    measured(serde_json::json!(raw), "C", Quantity::Temperature)
}

fn point_put(id_seed: u32, key: &str, seq: Seq, value: Option<Measured>) -> WalRecord {
    WalRecord::new(
        1,
        1_757_000_000_000 + id_seed as u64,
        WalRecordData::Point(PointOp {
            key: key.to_string(),
            seq,
            kind: PointKind::Put,
            value,
        }),
        None,
    )
}

fn point_delete(id_seed: u32, key: &str, seq: Seq) -> WalRecord {
    WalRecord::new(
        1,
        1_757_000_000_000 + id_seed as u64,
        WalRecordData::Point(PointOp { key: key.to_string(), seq, kind: PointKind::Delete, value: None }),
        None,
    )
}

fn range_del(id_seed: u32, name: &str, begin: &str, end: &str, seq: Seq) -> WalRecord {
    WalRecord::new(
        1,
        1_757_000_000_000 + id_seed as u64,
        WalRecordData::RangeDelete(RangeDelete {
            begin: begin.to_string(),
            end_exclusive: end.to_string(),
            seq,
            name: name.to_string(),
        }),
        None,
    )
}

fn params_v1() -> Params {
    Params {
        version: "params_v1".to_string(),
        note: "基线预算：每输出最多 5 条内部条目，用于强制演示输出切分".to_string(),
        budget: CompactionBudget {
            max_entries_per_output: 5,
            max_outputs: 8,
            size: measured(serde_json::json!(64), "KiB", Quantity::Bytes),
        },
    }
}

fn params_v2_tight() -> Params {
    Params {
        version: "params_v2_tight".to_string(),
        note: "故意收紧：每输出 1 条、最多 1 个输出，检验预算耗尽是独立结果".to_string(),
        budget: CompactionBudget {
            max_entries_per_output: 1,
            max_outputs: 1,
            size: measured(serde_json::json!(1), "KiB", Quantity::Bytes),
        },
    }
}

fn params_v2() -> Params {
    Params {
        version: "params_v2".to_string(),
        note: "参数修订：每输出放宽到 8 条，用于 L1->L2 压实".to_string(),
        budget: CompactionBudget {
            max_entries_per_output: 8,
            max_outputs: 8,
            size: measured(serde_json::json!(64), "KiB", Quantity::Bytes),
        },
    }
}

/// 构造案例包。
pub fn case_pack() -> CasePack {
    let basis = Basis::case_default();
    let records = vec![
        point_delete(3, "a", 3),
        point_put(4, "b", 4, celsius(21.5)),
        point_put(5, "c", 5, celsius(22.4)),
        range_del(6, "R1", "c", "e", 6),
        point_put(7, "d", 7, celsius(24.1)),
        point_delete(8, "d", 7),
        point_delete(9, "d", 8),
        point_delete(10, "e", 9),
        point_put(11, "b", 10, celsius(26.0)),
        point_put(12, "b", 11, celsius(25.2)),
        range_del(13, "R2", "f", "i", 12),
        point_put(14, "f", 13, celsius(19.8)),
        point_delete(15, "g", 14),
        point_put(16, "h", 15, measured(serde_json::json!("sensor-offline"), "C", Quantity::Temperature)),
    ];
    let wal_ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();

    let l1_base = SstFile {
        id: "sst_l1_base".to_string(),
        level: 1,
        smallest: "a".to_string(),
        largest: "e".to_string(),
        end_inclusive: true,
        entries: vec![
            Entry {
                entry_id: entry_id("lsm", "a", 1, OpType::Put),
                key: "a".to_string(),
                seq: 1,
                op: OpType::Put,
                value: celsius(20.0),
                origin: "lsm".to_string(),
            },
            Entry {
                entry_id: entry_id("lsm", "b", 2, OpType::Put),
                key: "b".to_string(),
                seq: 2,
                op: OpType::Put,
                value: celsius(21.0),
                origin: "lsm".to_string(),
            },
            Entry {
                entry_id: entry_id("lsm", "c", 1, OpType::Put),
                key: "c".to_string(),
                seq: 1,
                op: OpType::Put,
                value: celsius(19.5),
                origin: "lsm".to_string(),
            },
            Entry {
                entry_id: entry_id("lsm", "d", 2, OpType::Put),
                key: "d".to_string(),
                seq: 2,
                op: OpType::Put,
                value: celsius(22.0),
                origin: "lsm".to_string(),
            },
            Entry {
                entry_id: entry_id("lsm", "e", 1, OpType::Put),
                key: "e".to_string(),
                seq: 1,
                op: OpType::Put,
                value: celsius(18.0),
                origin: "lsm".to_string(),
            },
        ],
        range_fragments: Vec::new(),
        size_hint: Some(SizeHint { value: Measured::new(serde_json::json!(4), "KiB", Quantity::Bytes) }),
        origin: "lsm".to_string(),
    };

    let l2_old = SstFile {
        id: "sst_l2_old".to_string(),
        level: 2,
        smallest: "f".to_string(),
        largest: "g".to_string(),
        end_inclusive: true,
        entries: vec![
            Entry {
                entry_id: entry_id("lsm", "f", 1, OpType::Put),
                key: "f".to_string(),
                seq: 1,
                op: OpType::Put,
                value: celsius(17.0),
                origin: "lsm".to_string(),
            },
            Entry {
                entry_id: entry_id("lsm", "g", 1, OpType::Put),
                key: "g".to_string(),
                seq: 1,
                op: OpType::Put,
                value: celsius(18.5),
                origin: "lsm".to_string(),
            },
        ],
        range_fragments: Vec::new(),
        size_hint: Some(SizeHint { value: Measured::new(serde_json::json!(2), "KiB", Quantity::Bytes) }),
        origin: "lsm".to_string(),
    };

    let l3_ancient = SstFile {
        id: "sst_l3_ancient".to_string(),
        level: 3,
        smallest: "f".to_string(),
        largest: "g".to_string(),
        end_inclusive: true,
        entries: vec![
            Entry {
                entry_id: entry_id("lsm", "f", 0, OpType::Put),
                key: "f".to_string(),
                seq: 0,
                op: OpType::Put,
                value: celsius(15.0),
                origin: "lsm".to_string(),
            },
            Entry {
                entry_id: entry_id("lsm", "g", 0, OpType::Put),
                key: "g".to_string(),
                seq: 0,
                op: OpType::Put,
                value: celsius(16.0),
                origin: "lsm".to_string(),
            },
        ],
        range_fragments: Vec::new(),
        size_hint: Some(SizeHint { value: Measured::new(serde_json::json!(1), "KiB", Quantity::Bytes) }),
        origin: "lsm".to_string(),
    };

    let snap_old = Snapshot {
        id: "snap_old".to_string(),
        seq: 7,
        created_epoch_ms: 1_757_000_000_000,
        active: true,
        note: "活跃旧 snapshot：压实必须为它保留 seq<=7 的版本与 tombstone".to_string(),
    };
    let snap_new = Snapshot {
        id: "snap_new".to_string(),
        seq: 15,
        created_epoch_ms: 1_757_000_000_000,
        active: true,
        note: "最新 snapshot".to_string(),
    };

    let flush1_ids = wal_ids[0..7].to_vec();
    let flush2_ids = wal_ids[7..14].to_vec();
    let step_epoch = |n: u64| 1_757_000_000_000 + n * 1000;

    let steps = vec![
        CaseStep {
            ordinal: 1,
            title: "导入 WAL：点值/点删除/范围删除，含同序号多类型与不可转换值".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(1),
            command: Command::ImportWal { records },
            expect: vec![
                "14 条 WAL 记录全部保留原始表示与单位".into(),
                "(d, seq=7) 同时存在 PUT 与 DELETE".into(),
                "h 的 sensor-offline 无法转成温度，留在原位置并附诊断".into(),
            ],
            expect_error: None,
        },
        CaseStep {
            ordinal: 2,
            title: "登记多层 SSTable 摘要：L1 基底 [a,e] 与 L2 旧数据 [f,g]".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(2),
            command: Command::RegisterSst { file: l1_base },
            expect: vec!["L1 文件 sst_l1_base 入层".into()],
            expect_error: None,
        },
        CaseStep {
            ordinal: 3,
            title: "登记 L2 旧数据文件（不参与后续 L0->L1 压实）".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(3),
            command: Command::RegisterSst { file: l2_old },
            expect: vec!["L2 文件 sst_l2_old 入层".into()],
            expect_error: None,
        },
        CaseStep {
            ordinal: 4,
            title: "登记 L3 古老文件 [f,g]（不参与 C1，也不参与 C2）".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(4),
            command: Command::RegisterSst { file: l3_ancient },
            expect: vec!["R2/g@14 的下沉与保留将以该文件为外部旧数据依据".into()],
            expect_error: None,
        },
        CaseStep {
            ordinal: 4,
            title: "建立活跃旧 snapshot snap_old（seq=7）".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(4),
            command: Command::AddSnapshot { snapshot: snap_old },
            expect: vec!["seq<=7 的版本在压实中不可回收".into()],
            expect_error: None,
        },
        CaseStep {
            ordinal: 5,
            title: "建立最新 snapshot snap_new（seq=15）".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(5),
            command: Command::AddSnapshot { snapshot: snap_new },
            expect: vec!["所有已导入记录对 snap_new 可见".into()],
            expect_error: None,
        },
        CaseStep {
            ordinal: 6,
            title: "memtable flush #1 -> L0 文件（与后续 flush 的 key range 重叠）".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(6),
            command: Command::FlushMemtable {
                wal_ids: flush1_ids,
                level: 0,
                size_hint: Some(SizeHint { value: Measured::new(serde_json::json!(3), "KiB", Quantity::Bytes) }),
            },
            expect: vec![
                "L0 文件 range [a,e]".into(),
                "同序号多类型标记 same_seq_multi_type 落到 d".into(),
                "包含范围片段 R1 [c,e)".into(),
            ],
            expect_error: None,
        },
        CaseStep {
            ordinal: 7,
            title: "memtable flush #2 -> 与前一个重叠的 L0 文件 [b,i)".to_string(),
            branch: MAIN_BRANCH.to_string(),
            epoch_ms: step_epoch(7),
            command: Command::FlushMemtable {
                wal_ids: flush2_ids,
                level: 0,
                size_hint: Some(SizeHint { value: Measured::new(serde_json::json!(2.5), "KiB", Quantity::Bytes) }),
            },
            expect: vec!["两个 L0 文件 key range 重叠".into(), "包含范围片段 R2 [f,i)".into()],
            expect_error: None,
        },
    ];

    // 依赖 flush 产物身份：通过运行时确定性生成（引擎与测试共用同一个 case_pack）。
    let flush_file_ids = flush_file_ids(&steps);

    let mut steps = steps;
    let step_epoch2 = |n: u64| 1_757_000_000_000 + n * 1000;

    steps.push(CaseStep {
        ordinal: 8,
        title: "压实候选 C1：重叠 L0 + L1 基底，预算强制输出切分".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(8),
        command: Command::ProposeCompaction {
            job_name: "C1".to_string(),
            seed_file_id: flush_file_ids[0].clone(),
        },
        expect: vec![
            "选择理由同时包含 l0_overlapping 与 output_level_overlap".into(),
            "R1 因活跃旧 snapshot 保守保留并按输出边界裁剪为 [c,d)/[d,e)".into(),
            "b@10 被 b@11 遮蔽且旧 snapshot 不需要它 -> shadowed_in_compaction".into(),
            "输出在 key 边界切分，同层互不重叠".into(),
        ],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 9,
        title: "压实中断：C1 在写入第 2/3 个输出时中断".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(9),
        command: Command::InterruptCompaction {
            compaction_id: "C1".to_string(),
            phase: "writing_output:2".to_string(),
        },
        expect: vec!["C1 状态变为 interrupted，候选输出仍保留".into()],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 10,
        title: "中断恢复后安装 C1".to_string().into(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(10),
        command: Command::InstallCompaction { compaction_id: "C1".to_string() },
        expect: vec![
            "输入文件被移除、输出文件进入 L1".into(),
            "L2 sst_l2_old 未参与、未被修改".into(),
        ],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 11,
        title: "重复安装同一结果：必须被拒绝而不是产生重复文件".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(11),
        command: Command::InstallCompaction { compaction_id: "C1".to_string() },
        expect: vec!["返回 duplicate_install_rejected，文件集合不变".into()],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 12,
        title: "参数修订 v2（先故意收紧预算，制造预算耗尽反例）".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(12),
        command: Command::ReviseParams { params: params_v2_tight() },
        expect: vec!["新参数版本 params_v2_tight 生效".into()],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 13,
        title: "反例：预算耗尽——C_probe 在紧预算下无合法输出切分".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(13),
        command: Command::ProposeCompaction {
            job_name: "C_probe".to_string(),
            seed_file_id: "sst_l2_old".to_string(),
        },
        expect: vec!["失败事件 class=budget_exhausted，含位置与所需/可用容量".into()],
        expect_error: Some("budget_exhausted".into()),
    });
    steps.push(CaseStep {
        ordinal: 14,
        title: "参数修订恢复到 params_v2（每输出 8 条）".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(14),
        command: Command::ReviseParams { params: params_v2() },
        expect: vec!["params_v2 成为当前版本".into()],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 15,
        title: "压实候选 C2：C1 的 [e,h] 输出与 L2 旧数据合并，R2 下沉到 L2".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(15),
        command: Command::ProposeCompaction {
            job_name: "C2".to_string(),
            seed_file_id: c1_output_id(&steps, 3),
        },
        expect: vec![
            "输入同时包含 C1 输出与 sst_l2_old（range_keep_older_data_outside 的证据）".into(),
            "R2 保留并下沉到 L2 单输出 [e,h]".into(),
        ],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 16,
        title: "人工决定：接受 C2".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(16),
        command: Command::Decide {
            target_id: "C2".to_string(),
            action: DecideAction::Accept,
            note: "审阅人 A：确认输出边界与 tombstone 下沉".to_string(),
        },
        expect: vec!["C2 安装".into()],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 17,
        title: "互相冲突的人工决定：在已安装后再否定 C2（被采纳与未采纳结果都保留）".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(17),
        command: Command::Decide {
            target_id: "C2".to_string(),
            action: DecideAction::Reject,
            note: "审阅人 B：认为边界应重切；与安装事实冲突".to_string(),
        },
        expect: vec!["决定被保留，conflict=target_already_installed，状态不回滚".into()],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 18,
        title: "只改动一个直接来源：修订 WAL b@4 的值（局部依赖）".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(18),
        command: Command::FixSource {
            record_id: wal_ids[1].clone(),
            new_data: WalRecordData::Point(PointOp {
                key: "b".to_string(),
                seq: 4,
                kind: PointKind::Put,
                value: celsius(23.9),
            }),
            epoch_ms: 1_757_000_000_000,
        },
        expect: vec![
            "旧 WAL 版本保留并标记 superseded_by".into(),
            "stale 只覆盖 flush#1 与 C1，不覆盖 C2（影响被限制在正确范围）".into(),
        ],
        expect_error: None,
    });
    steps.push(CaseStep {
        ordinal: 19,
        title: "反例：格式损坏——导入缺 key 的点记录".to_string(),
        branch: MAIN_BRANCH.to_string(),
        epoch_ms: step_epoch2(19),
        command: Command::ImportWal {
            records: vec![WalRecord::new(
                1,
                step_epoch2(19),
                WalRecordData::Point(PointOp {
                    key: String::new(),
                    seq: 20,
                    kind: PointKind::Put,
                    value: celsius(1.0),
                }),
                None,
            )],
        },
        expect: vec!["失败事件 class=corrupt，诊断带位置 command.records[0]".into()],
        expect_error: Some("corrupt".into()),
    });
    steps.push(CaseStep {
        ordinal: 20,
        title: "分叉出 read-semantics 分支".to_string(),
        branch: READ_BRANCH.to_string(),
        epoch_ms: step_epoch2(20),
        command: Command::ForkBranch { branch: READ_BRANCH.to_string() },
        expect: vec!["后续探针挂在 read-semantics 分支".into()],
        expect_error: None,
    });

    let probes: Vec<(u32, &str, Command, Vec<String>)> = vec![
        (21, "point d @ snap_old(7)：同序号多类型且恰为最新可见版本 -> 保留全部合法候选",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "d".to_string(), snapshot_id: "snap_old".to_string() } },
            vec!["kind=ambiguous_same_seq_multi_type".into(), "候选含 delete_wins_by_internal_order 与 put_survives".into()]),
        (22, "point c @ snap_old(7)：R1@6 对旧 snapshot 不存在",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "c".to_string(), snapshot_id: "snap_old".to_string() } },
            vec!["kind=value seq=5".into()]),
        (23, "point c @ snap_new：R1 遮蔽更旧点数据（端点 begin 含、end 不含）",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "c".to_string(), snapshot_id: "snap_new".to_string() } },
            vec!["kind=range_tombstone name=R1".into()]),
        (24, "point e @ snap_old 与 snap_new：端点外/点 tombstone 对比",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "e".to_string(), snapshot_id: "snap_old".to_string() } },
            vec!["kind=value seq=1（R1 的 end_exclusive=e，不含 e）".into()]),
        (25, "point g @ snap_old：DELETE@14 不可见",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "g".to_string(), snapshot_id: "snap_old".to_string() } },
            vec!["kind=value seq=1".into()]),
        (26, "point b @ snap_old：仅 seq<=7 版本可见",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "b".to_string(), snapshot_id: "snap_old".to_string() } },
            vec!["kind=value seq=4".into()]),
        (27, "point h @ snap_new：不可转换温度原值保留",
            Command::ReadProbe { probe: ProbeSpec::PointLookup { key: "h".to_string(), snapshot_id: "snap_new".to_string() } },
            vec!["kind=value raw=sensor-offline 且 diagnostics 非空".into()]),
        (28, "range scan [a,i) @ snap_new：完整版本链按内部键顺序",
            Command::ReadProbe { probe: ProbeSpec::RangeScan { begin: "a".to_string(), end_exclusive: "i".to_string(), snapshot_id: "snap_new".to_string() } },
            vec!["a=point_tombstone c=range_tombstone d 带 ambiguous 标记".into()]),
        (29, "tombstone 覆盖报告 @ snap_new",
            Command::ReadProbe { probe: ProbeSpec::Coverage { snapshot_id: "snap_new".to_string() } },
            vec!["报告列出各片段 begin/end/seq 与遮蔽的 entry 身份".into()]),
    ];
    for (ordinal, title, command, expect) in probes {
        steps.push(CaseStep {
            ordinal,
            title: title.to_string(),
            branch: READ_BRANCH.to_string(),
            epoch_ms: step_epoch2(ordinal as u64),
            command,
            expect,
            expect_error: None,
        });
    }

    for (idx, step) in steps.iter_mut().enumerate() {
        step.ordinal = idx as u32 + 1;
    }

    let invariants = vec![
        InvariantEvent {
            id: "inv_internal_key_visibility".to_string(),
            rule: "internal_key_order_and_snapshot_visibility".to_string(),
            statement: "读取按内部键顺序（key 升序、seq 降序、type 降序），并以 snapshot 持有的序号判可见性（entry.seq <= snapshot.seq）。"
                .to_string(),
            witnesses: vec!["probe#22".into(), "probe#23".into(), "probe#25".into(), "probe#26".into()],
        },
        InvariantEvent {
            id: "inv_range_tombstone_no_cross".to_string(),
            rule: "range_tombstone_cannot_cross_uncompacted_old_data".to_string(),
            statement: "范围删除不能越过未参与压实的旧数据而提前丢弃：活跃旧 snapshot 或更深层旧点数据存在时，tombstone 必须保守保留并下沉。"
                .to_string(),
            witnesses: vec!["C1".into(), "C2".into(), "sst_l2_old".into(), "snap_old".into()],
        },
        InvariantEvent {
            id: "inv_output_nonoverlap".to_string(),
            rule: "output_splitting_preserves_nonoverlapping_level".to_string(),
            statement: "输出切分只能在 key 边界进行；同层输出必须互不重叠，违反时语义无解并保留候选。"
                .to_string(),
            witnesses: vec!["C1".into(), "params_v1".into()],
        },
        InvariantEvent {
            id: "inv_three_error_classes".to_string(),
            rule: "budget_vs_corrupt_vs_unresolved_are_distinct".to_string(),
            statement: "资源预算耗尽、格式损坏、语义无解是三种不同结果（判别式不同、HTTP 状态不同、载荷不同）。"
                .to_string(),
            witnesses: vec!["step#14".into(), "step#20".into(), "probe#21".into()],
        },
    ];

    CasePack {
        format: CASE_FORMAT.to_string(),
        title: "层合迹：LSM tombstone、范围删除与压实选择审阅".to_string(),
        description: "存储团队导入 WAL、memtable flush 与多层 SSTable 摘要，模拟 point/range tombstone \
在 compaction 中的保留和下沉。第二分支专门审阅内部键顺序 + snapshot 可见性。"
            .to_string(),
        basis,
        initial_params: params_v1(),
        steps,
        invariants: Vec::new(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepReport {
    pub ordinal: u32,
    pub title: String,
    pub ok: bool,
    pub error_class: Option<String>,
    pub event_id: Option<String>,
}

fn run_steps(engine: &mut Engine, steps: &[CaseStep]) -> Vec<StepReport> {
    let mut reports = Vec::new();
    for step in steps {
        let result = engine.apply(step.command.clone(), &step.branch, step.epoch_ms);
        match (result, &step.expect_error) {
            (Ok(event), None) => reports.push(StepReport {
                ordinal: step.ordinal,
                title: step.title.clone(),
                ok: true,
                error_class: None,
                event_id: Some(event.id),
            }),
            (Err(error), Some(expected)) if error.class() == expected => reports.push(StepReport {
                ordinal: step.ordinal,
                title: step.title.clone(),
                ok: true,
                error_class: Some(error.class().to_string()),
                event_id: engine.log.last().map(|e| e.id.clone()),
            }),
            (Err(error), expected) => reports.push(StepReport {
                ordinal: step.ordinal,
                title: step.title.clone(),
                ok: false,
                error_class: Some(format!("{:?}(expect {expected:?})", error.class())),
                event_id: None,
            }),
            (Ok(event), Some(expected)) => reports.push(StepReport {
                ordinal: step.ordinal,
                title: step.title.clone(),
                ok: false,
                error_class: Some(format!("succeeded but expected {expected}")),
                event_id: Some(event.id),
            }),
        }
    }
    reports
}

/// 只运行导入/登记/snapshot/flush 步骤（1..=7），用于确定性拿到 flush 文件身份。
fn flush_file_ids(steps: &[CaseStep]) -> Vec<String> {
    let mut engine = Engine::new(Basis::case_default(), params_v1());
    for step in steps
        .iter()
        .take_while(|s| !matches!(s.command, Command::ProposeCompaction { .. }))
    {
        engine.apply(step.command.clone(), &step.branch, step.epoch_ms).expect("base steps ok");
    }
    engine
        .state
        .files
        .iter()
        .filter(|f| f.level == 0)
        .map(|f| f.id.clone())
        .collect()
}

fn c1_output_id(steps: &[CaseStep], output_index: u32) -> String {
    let mut engine = Engine::new(Basis::case_default(), params_v1());
    for step in steps.iter().take_while(|s| {
        !matches!(&s.command, Command::ProposeCompaction { job_name, .. } if job_name == "C2")
    }) {
        let _ = engine.apply(step.command.clone(), &step.branch, step.epoch_ms);
    }
    let comp = engine
        .state
        .compactions
        .iter()
        .find(|c| c.job_name == "C1")
        .expect("C1 proposed");
    comp.outputs[(output_index - 1) as usize].file_id.clone()
}

/// 运行整个离线案例包；返回每步是否符合预期（失败尝试也算作“预期内”的步骤）。
pub fn run_case(pack: &CasePack) -> (Engine, Vec<StepReport>) {
    let mut engine = Engine::new(pack.basis.clone(), pack.initial_params.clone());
    let reports = run_steps(&mut engine, &pack.steps);
    (engine, reports)
}

/// 仅用于测试：判定所有案例步骤是否都满足预期。
pub fn all_ok(reports: &[StepReport]) -> bool {
    reports.iter().all(|r| r.ok)
}
