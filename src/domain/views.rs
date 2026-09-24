//! 复核界面的只读投影：层级文件、key 版本链、tombstone 覆盖、压实选择理由、输出边界。
//! 每个显示值都带稳定身份；历史前缀投影用于“回到任意旧事件”。

use crate::domain::engine::{Event, State};
use crate::domain::model::*;
use crate::domain::units::Basis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectView {
    pub basis: Option<Basis>,
    pub branch: String,
    pub head_ordinal: u64,
    pub head_event_id: Option<String>,
    pub params: Vec<ParamsView>,
    pub files: Vec<FileView>,
    pub key_chains: Vec<KeyChainView>,
    pub tombstones: Vec<TombstoneView>,
    pub compactions: Vec<CompactionView>,
    pub decisions: Vec<DecisionView>,
    pub snapshots: Vec<Snapshot>,
    pub wal: Vec<WalView>,
    pub flags: Vec<FlagView>,
    pub stale: Vec<StaleView>,
    pub events: Vec<EventSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamsView {
    pub version: String,
    pub note: String,
    pub budget: CompactionBudget,
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileView {
    pub id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    pub end_inclusive: bool,
    pub entry_count: u32,
    pub range_fragment_count: u32,
    pub origin: String,
    pub size: Option<String>,
    pub entry_diagnostics: Vec<EntryDiagnosticView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDiagnosticView {
    pub entry_id: String,
    pub key: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionView {
    pub entry_id: String,
    pub seq: Seq,
    pub op: String,
    pub value_display: Option<String>,
    pub value_raw: Option<serde_json::Value>,
    pub value_unit: Option<String>,
    pub value_diagnostics: Vec<String>,
    pub file_id: String,
    pub level: u32,
    pub origin: String,
    pub visible_at: BTreeMap<String, bool>,
    pub valid_interval: String,
    pub invalid_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyChainView {
    pub key: String,
    pub versions: Vec<VersionView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombstoneView {
    pub fragment_id: String,
    pub name: String,
    pub seq: Seq,
    pub begin: String,
    pub end_exclusive: String,
    pub file_id: String,
    pub level: u32,
    pub retention: Vec<String>,
    pub covered_entries: Vec<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionView {
    pub id: String,
    pub job_name: String,
    pub target_level: u32,
    pub status: String,
    pub installed_by: Option<String>,
    pub inputs: Vec<String>,
    pub pick_reasons: Vec<PickReason>,
    pub outputs: Vec<OutputView>,
    pub range_retention: Vec<RangeRetentionNote>,
    pub provenance: Provenance,
    pub stale: bool,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputView {
    pub file_id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    pub end_inclusive: bool,
    pub entry_count: u32,
    pub split: Option<SplitBoundary>,
    pub drops: Vec<DropReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionView {
    pub id: String,
    pub target_id: String,
    pub action: String,
    pub note: String,
    pub conflict: Option<String>,
    pub event_id: String,
    pub ordinal: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalView {
    pub id: String,
    pub version: u32,
    pub seq: Seq,
    pub summary: String,
    pub current: bool,
    pub superseded_by: Option<String>,
    pub revised_from: Option<String>,
    pub value_diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlagView {
    pub id: String,
    pub kind: String,
    pub key: String,
    pub seq: Seq,
    pub detail: String,
    pub wal_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleView {
    pub object_id: String,
    pub object_kind: String,
    pub reason: String,
    pub marked_by_event: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSummary {
    pub id: String,
    pub parent: Option<String>,
    pub ordinal: u64,
    pub branch: String,
    pub kind: String,
    pub epoch_ms: u64,
    pub is_error: bool,
    pub summary: String,
}

use crate::domain::engine::{Command, Decision, Outcome};

pub fn build_view(st: &State, log: &[Event], branch: &str) -> ProjectView {
    let current_params = st.params.last().map(|p| p.version.clone());
    let params = st
        .params
        .iter()
        .map(|p| ParamsView {
            version: p.version.clone(),
            note: p.note.clone(),
            budget: p.budget.clone(),
            current: current_params.as_deref() == Some(p.version.as_str()),
        })
        .collect();

    let stale_ids: std::collections::BTreeSet<String> = st.stale.iter().map(|m| m.object_id.clone()).collect();

    let files = st
        .files
        .iter()
        .map(|f| {
            let entry_diagnostics = f
                .entries
                .iter()
                .filter_map(|e| {
                    e.value.as_ref().and_then(|v| {
                        if v.diagnostics.is_empty() {
                            None
                        } else {
                            Some(EntryDiagnosticView {
                                entry_id: e.entry_id.clone(),
                                key: e.key.clone(),
                                message: v.diagnostics.join("; "),
                            })
                        }
                    })
                })
                .collect();
            FileView {
                id: f.id.clone(),
                level: f.level,
                smallest: f.smallest.clone(),
                largest: f.largest.clone(),
                end_inclusive: f.end_inclusive,
                entry_count: f.entries.len() as u32,
                range_fragment_count: f.range_fragments.len() as u32,
                origin: f.origin.clone(),
                size: f.size_hint.as_ref().map(|s| s.value.display()),
                entry_diagnostics,
            }
        })
        .collect();

    // key 版本链：跨文件汇总内部条目，按内部键顺序。
    let mut chain: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for file in &st.files {
        for entry in &file.entries {
            chain.entry(entry.key.clone()).or_default().push(entry);
        }
    }
    let active_snaps: Vec<&Snapshot> = st.snapshots.iter().filter(|s| s.active).collect();
    let mut key_chains = Vec::new();
    for (key, mut entries) in chain {
        entries.sort_by(|a, b| {
            b.seq.cmp(&a.seq).then_with(|| b.op.cmp(&a.op)).then_with(|| a.entry_id.cmp(&b.entry_id))
        });
        let versions = entries
            .iter()
            .map(|e| {
                let file = st.files.iter().find(|f| f.entries.iter().any(|x| x.entry_id == e.entry_id));
                let mut visible_at = BTreeMap::new();
                for snap in &active_snaps {
                    visible_at.insert(snap.id.clone(), e.seq <= snap.seq);
                }
                VersionView {
                    entry_id: e.entry_id.clone(),
                    seq: e.seq,
                    op: e.op.label().to_string(),
                    value_display: e.value.as_ref().map(|v| v.display()),
                    value_raw: e.value.as_ref().map(|v| v.raw.clone()),
                    value_unit: e.value.as_ref().map(|v| v.raw_unit.clone()),
                    value_diagnostics: e.value.as_ref().map(|v| v.diagnostics.clone()).unwrap_or_default(),
                    file_id: file.map(|f| f.id.clone()).unwrap_or_default(),
                    level: file.map(|f| f.level).unwrap_or_default(),
                    origin: e.origin.clone(),
                    visible_at,
                    valid_interval: format!("seq <= 当前 snapshot 序号时可见；entry seq = {}", e.seq),
                    invalid_reason: match e.op {
                        OpType::Delete => Some("点 tombstone：读取到此处即停止沿版本链向下".to_string()),
                        OpType::RangeDelBegin | OpType::RangeDelEnd => {
                            Some("范围删除标记：按 [begin,end) 遮蔽更旧点数据".to_string())
                        }
                        OpType::Put => None,
                    },
                }
            })
            .collect();
        key_chains.push(KeyChainView { key, versions });
    }

    // tombstone 覆盖：保留在文件中的片段 + 被安全回收的记录。
    let mut tombstones = Vec::new();
    for file in &st.files {
        for frag in &file.range_fragments {
            let covered_entries = file
                .entries
                .iter()
                .filter(|e| {
                    e.op == OpType::Put
                        && e.seq < frag.seq
                        && e.key >= frag.begin
                        && e.key < frag.end_exclusive
                })
                .map(|e| e.entry_id.clone())
                .collect();
            tombstones.push(TombstoneView {
                fragment_id: frag.fragment_id.clone(),
                name: frag.name.clone(),
                seq: frag.seq,
                begin: frag.begin.clone(),
                end_exclusive: frag.end_exclusive.clone(),
                file_id: file.id.clone(),
                level: file.level,
                retention: frag.retention.clone(),
                covered_entries,
                status: "retained".to_string(),
            });
        }
    }
    for comp in &st.compactions {
        for out in &comp.outputs {
            for drop in &out.drops {
                if matches!(drop.op, OpType::RangeDelBegin | OpType::RangeDelEnd) {
                    tombstones.push(TombstoneView {
                        fragment_id: format!("dropped:{}", drop.entry_id),
                        name: drop.entry_id.clone(),
                        seq: drop.seq,
                        begin: drop.key.clone(),
                        end_exclusive: String::new(),
                        file_id: out.file_id.clone(),
                        level: out.level,
                        retention: Vec::new(),
                        covered_entries: Vec::new(),
                        status: format!("dropped:{}", drop.reason_code),
                    });
                }
            }
        }
    }

    let compactions = st
        .compactions
        .iter()
        .map(|c| CompactionView {
            id: c.id.clone(),
            job_name: c.job_name.clone(),
            target_level: c.target_level,
            status: status_label(c.status),
            installed_by: c.installed_by.clone(),
            inputs: c.inputs.clone(),
            pick_reasons: c.pick_reasons.clone(),
            outputs: c
                .outputs
                .iter()
                .map(|o| OutputView {
                    file_id: o.file_id.clone(),
                    level: o.level,
                    smallest: o.smallest.clone(),
                    largest: o.largest.clone(),
                    end_inclusive: o.end_inclusive,
                    entry_count: o.entry_count,
                    split: o.split.clone(),
                    drops: o.drops.clone(),
                })
                .collect(),
            range_retention: c.range_retention.clone(),
            provenance: c.provenance.clone(),
            stale: stale_ids.contains(&c.id),
            diagnostics: c.diagnostics.clone(),
        })
        .collect();

    let decisions = st
        .decisions
        .iter()
        .map(|d: &Decision| DecisionView {
            id: d.id.clone(),
            target_id: d.target_id.clone(),
            action: d.action.clone(),
            note: d.note.clone(),
            conflict: d.conflict.clone(),
            event_id: d.event_id.clone(),
            ordinal: d.ordinal,
        })
        .collect();

    let wal = st
        .wal
        .iter()
        .map(|w| {
            let (summary, value_diagnostics) = match &w.data {
                WalRecordData::Point(p) => (
                    format!("seq={} {} key={}", p.seq, p.op_type().label(), p.key),
                    p.value.as_ref().map(|v| v.diagnostics.clone()).unwrap_or_default(),
                ),
                WalRecordData::RangeDelete(r) => (
                    format!("seq={} RANGE_DEL {} [{}, {})", r.seq, r.name, r.begin, r.end_exclusive),
                    Vec::new(),
                ),
            };
            WalView {
                id: w.id.clone(),
                version: w.version,
                seq: w.data.seq(),
                summary,
                current: w.current(),
                superseded_by: w.superseded_by.clone(),
                revised_from: w.revised_from.clone(),
                value_diagnostics,
            }
        })
        .collect();

    let flags = st
        .flags
        .iter()
        .map(|f| FlagView {
            id: f.id.clone(),
            kind: f.kind.clone(),
            key: f.key.clone(),
            seq: f.seq,
            detail: f.detail.clone(),
            wal_ids: f.wal_ids.clone(),
        })
        .collect();

    let stale = st
        .stale
        .iter()
        .map(|m| StaleView {
            object_id: m.object_id.clone(),
            object_kind: m.object_kind.clone(),
            reason: m.reason.clone(),
            marked_by_event: m.marked_by_event.clone(),
        })
        .collect();

    let events = log
        .iter()
        .filter(|e| branch == "*" || e.branch == branch)
        .map(|e| EventSummary {
            id: e.id.clone(),
            parent: e.parent.clone(),
            ordinal: e.ordinal,
            branch: e.branch.clone(),
            kind: e.kind(),
            epoch_ms: e.epoch_ms,
            is_error: matches!(e.outcome, Outcome::Error { .. }),
            summary: summarize_event(e),
        })
        .collect();

    let head = log.last();
    ProjectView {
        basis: st.basis.clone(),
        branch: branch.to_string(),
        head_ordinal: head.map(|e| e.ordinal).unwrap_or(0),
        head_event_id: head.map(|e| e.id.clone()),
        params,
        files,
        key_chains,
        tombstones,
        compactions,
        decisions,
        snapshots: st.snapshots.clone(),
        wal,
        flags,
        stale,
        events,
    }
}

fn status_label(status: CompactionStatus) -> String {
    match status {
        CompactionStatus::Proposed => "proposed",
        CompactionStatus::Interrupted => "interrupted",
        CompactionStatus::Installed => "installed",
        CompactionStatus::Rejected => "rejected",
    }
    .to_string()
}

fn summarize_event(event: &Event) -> String {
    match (&event.command, &event.outcome) {
        (Command::ImportWal { records }, _) => format!("导入 WAL {} 条", records.len()),
        (Command::RegisterSst { file }, _) => format!("登记 {} L{} [{},{}]", file.id, file.level, file.smallest, file.largest),
        (Command::AddSnapshot { snapshot }, _) => format!("snapshot {} seq={}", snapshot.id, snapshot.seq),
        (Command::CloseSnapshot { snapshot_id }, _) => format!("关闭 snapshot {snapshot_id}"),
        (Command::FlushMemtable { wal_ids, level, .. }, _) => format!("flush {} 条 WAL -> L{level}", wal_ids.len()),
        (Command::ProposeCompaction { job_name, .. }, Outcome::CompactionProposed { compaction }) => {
            format!("压实候选 {job_name}：{} 输入 -> {} 输出", compaction.inputs.len(), compaction.outputs.len())
        }
        (Command::ProposeCompaction { job_name, .. }, Outcome::Error { error }) => {
            format!("压实 {job_name} 失败：{}", error.class())
        }
        (Command::InterruptCompaction { compaction_id, phase }, _) => {
            format!("压实 {compaction_id} 中断于 {phase}")
        }
        (Command::InstallCompaction { compaction_id }, Outcome::CompactionInstalled { .. }) => {
            format!("安装压实 {compaction_id}")
        }
        (Command::InstallCompaction { compaction_id }, Outcome::DuplicateInstallRejected { .. }) => {
            format!("拒绝重复安装 {compaction_id}")
        }
        (Command::Decide { target_id, note, .. }, Outcome::DecisionRecorded { conflict, .. }) => match conflict {
            Some(c) => format!("对 {target_id} 的冲突决定（{c}）：{note}"),
            None => format!("对 {target_id} 的决定：{note}"),
        },
        (Command::ReviseParams { params }, _) => format!("参数修订 -> {}", params.version),
        (Command::FixSource { record_id, .. }, _) => format!("修订源数据 {record_id}"),
        (Command::ReadProbe { probe }, _) => format!("读取探针 {probe:?}"),
        (Command::ForkBranch { branch }, _) => format!("分支 {branch}"),
        (_, Outcome::Error { error }) => format!("失败尝试：{} @ {}", error.class(), error.pos().path),
        _ => format!("{:?}", event.command),
    }
}
