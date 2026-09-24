//! 事件溯源引擎：所有材料、计算结果与决定都是追加式事件。
//!
//! - 每个事件有稳定身份（由父事件 + 分支 + 载荷决定，与墙钟无关）。
//! - 重放 = 依序应用事件；回到任意旧事件 = 重放到该事件的 ordinal。
//! - 失败尝试也记录为事件（outcome=error），留下输入片段、判定状态与受影响对象。

use crate::domain::compact::run_compaction;
use crate::domain::errors::{corrupt, LoadError, LoadResult};
use crate::domain::ids::stable_id;
use crate::domain::internals;
use crate::domain::model::*;
use crate::domain::units::Basis;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flag {
    pub id: String,
    pub kind: String,
    pub key: String,
    pub seq: Seq,
    pub detail: String,
    pub wal_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleMark {
    pub object_id: String,
    pub object_kind: String,
    pub reason: String,
    pub marked_by_event: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub target_id: String,
    pub action: String,
    pub note: String,
    pub conflict: Option<String>,
    pub event_id: String,
    pub ordinal: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub basis: Option<Basis>,
    pub params: Vec<Params>,
    pub wal: Vec<WalRecord>,
    pub files: Vec<SstFile>,
    pub snapshots: Vec<Snapshot>,
    pub flushes: Vec<FlushRecord>,
    pub compactions: Vec<CompactionResult>,
    pub decisions: Vec<Decision>,
    pub flags: Vec<Flag>,
    pub stale: Vec<StaleMark>,
}

impl State {
    pub fn current_params(&self) -> LoadResult<&Params> {
        self.params
            .last()
            .ok_or_else(|| corrupt("state.params", "参数版本为空：引擎尚未初始化"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    ImportWal { records: Vec<WalRecord> },
    RegisterSst { file: SstFile },
    AddSnapshot { snapshot: Snapshot },
    CloseSnapshot { snapshot_id: String },
    FlushMemtable { wal_ids: Vec<String>, level: u32, size_hint: Option<SizeHint> },
    ProposeCompaction { job_name: String, seed_file_id: String },
    InterruptCompaction { compaction_id: String, phase: String },
    InstallCompaction { compaction_id: String },
    Decide { target_id: String, action: DecideAction, note: String },
    ReviseParams { params: Params },
    FixSource { record_id: String, new_data: WalRecordData, epoch_ms: u64 },
    ReadProbe { probe: ProbeSpec },
    ForkBranch { branch: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DecideAction {
    Accept,
    Reject,
    AdjustParams { params: Params },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "probe", rename_all = "snake_case")]
pub enum ProbeSpec {
    PointLookup { key: String, snapshot_id: String },
    RangeScan { begin: String, end_exclusive: String, snapshot_id: String },
    Coverage { snapshot_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    WalImported { ids: Vec<String> },
    SstRegistered { file_id: String },
    SnapshotAdded { id: String },
    SnapshotClosed { id: String },
    Flushed { flush_id: String, file_id: String },
    CompactionProposed { compaction: CompactionResult },
    CompactionInterrupted { compaction_id: String, phase: String },
    CompactionInstalled { compaction_id: String, outputs: Vec<String>, removed: Vec<String> },
    DuplicateInstallRejected { compaction_id: String, first_installed_by: String },
    DecisionRecorded { decision_id: String, conflict: Option<String> },
    ParamsRevised { version: String },
    SourceFixed { new_record_id: String, stale: Vec<String> },
    ProbeResult { probe_id: String, result: serde_json::Value },
    BranchForked { branch: String },
    /// 失败尝试：输入片段、判定状态（三类错误之一）与位置全部保留。
    Error { error: LoadError },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub parent: Option<String>,
    pub ordinal: u64,
    pub branch: String,
    pub epoch_ms: u64,
    pub command: Command,
    pub outcome: Outcome,
}

impl Event {
    pub fn kind(&self) -> String {
        match &self.command {
            Command::ImportWal { .. } => "import_wal",
            Command::RegisterSst { .. } => "register_sst",
            Command::AddSnapshot { .. } => "add_snapshot",
            Command::CloseSnapshot { .. } => "close_snapshot",
            Command::FlushMemtable { .. } => "flush_memtable",
            Command::ProposeCompaction { .. } => "propose_compaction",
            Command::InterruptCompaction { .. } => "interrupt_compaction",
            Command::InstallCompaction { .. } => "install_compaction",
            Command::Decide { .. } => "decide",
            Command::ReviseParams { .. } => "revise_params",
            Command::FixSource { .. } => "fix_source",
            Command::ReadProbe { .. } => "read_probe",
            Command::ForkBranch { .. } => "fork_branch",
        }
        .to_string()
    }
}

pub struct Engine {
    pub state: State,
    pub log: Vec<Event>,
}

impl Engine {
    pub fn new(basis: Basis, params: Params) -> Self {
        Self {
            state: State { basis: Some(basis), params: vec![params], ..Default::default() },
            log: Vec::new(),
        }
    }

    pub fn head(&self) -> Option<&Event> {
        self.log.last()
    }

    fn next_event_id(&self, branch: &str, command: &Command) -> String {
        let parent = self.log.last().map(|e| e.id.clone());
        stable_id("evt", &serde_json::json!({"parent": parent, "branch": branch, "command": command}))
    }

    /// 应用一条命令。失败也会留下 Error 事件（审计轨迹），同时把错误返回给调用方。
    pub fn apply(&mut self, command: Command, branch: &str, epoch_ms: u64) -> LoadResult<Event> {
        let event_id = self.next_event_id(branch, &command);
        let ordinal = self.log.len() as u64 + 1;
        let outcome = match self.compute(&command, &event_id, ordinal) {
            Ok(outcome) => outcome,
            Err(error) => {
                let event = Event {
                    id: event_id,
                    parent: self.log.last().map(|e| e.id.clone()),
                    ordinal,
                    branch: branch.to_string(),
                    epoch_ms,
                    command,
                    outcome: Outcome::Error { error: error.clone() },
                };
                self.log.push(event);
                return Err(error);
            }
        };
        let event = Event {
            id: event_id.clone(),
            parent: self.log.last().map(|e| e.id.clone()),
            ordinal,
            branch: branch.to_string(),
            epoch_ms,
            command,
            outcome,
        };
        mutate(&mut self.state, &event);
        self.log.push(event.clone());
        Ok(event)
    }

    /// 重放事件流（导入/恢复/回到旧事件共用）。信任事件中记录的结果，保证历史结论不被新计算覆盖。
    pub fn replay(basis: Basis, params: Params, events: &[Event]) -> Engine {
        let mut engine = Engine::new(basis, params);
        for event in events {
            if matches!(event.outcome, Outcome::Error { .. }) {
                engine.log.push(event.clone());
                continue;
            }
            mutate(&mut engine.state, event);
            engine.log.push(event.clone());
        }
        engine
    }

    /// 回到任意旧事件：重放前缀，恢复当时的依赖版本与可见结论。
    pub fn state_at(&self, ordinal: u64) -> State {
        let mut state = State { basis: self.state.basis.clone(), params: Vec::new(), ..Default::default() };
        for event in self.log.iter().filter(|e| e.ordinal <= ordinal) {
            if matches!(event.outcome, Outcome::Error { .. }) {
                continue;
            }
            mutate(&mut state, event);
        }
        state
    }
}

impl Engine {
    fn compute(&self, command: &Command, event_id: &str, ordinal: u64) -> LoadResult<Outcome> {
        let st = &self.state;
        match command {
            Command::ImportWal { records } => {
                if records.is_empty() {
                    return Err(corrupt("command.records", "导入批次为空：输入不完整，不能伪装成功"));
                }
                for (idx, record) in records.iter().enumerate() {
                    let path = format!("command.records[{idx}]");
                    if record.data.seq() == 0 {
                        return Err(corrupt(path, "sequence number 必须 >= 1"));
                    }
                    match &record.data {
                        WalRecordData::Point(p) if p.key.is_empty() => {
                            return Err(corrupt(path, "point record 缺少 key"));
                        }
                        WalRecordData::Point(p) if p.kind == PointKind::Put && p.value.is_none() => {
                            return Err(corrupt(format!("{path}.value"), "PUT 记录缺少 value（禁止用空值伪装）"));
                        }
                        WalRecordData::RangeDelete(r) if r.begin >= r.end_exclusive => {
                            return Err(corrupt(
                                path,
                                format!("range delete 端点非法：begin {} >= end_exclusive {}", r.begin, r.end_exclusive),
                            ));
                        }
                        _ => {}
                    }
                    if st.wal.iter().any(|w| w.id == record.id) {
                        return Err(corrupt(path, format!("WAL 记录 {} 已导入（材料不可变）", record.id)));
                    }
                }
                let mut seen = std::collections::BTreeSet::new();
                for record in records {
                    if !seen.insert(record.id.clone()) {
                        return Err(corrupt("command.records", format!("批次内出现重复记录 {}", record.id)));
                    }
                }
                Ok(Outcome::WalImported { ids: records.iter().map(|r| r.id.clone()).collect() })
            }
            Command::RegisterSst { file } => {
                if file.smallest > file.largest {
                    return Err(corrupt("command.file.range", "SSTable smallest > largest"));
                }
                if file.id.is_empty() || st.files.iter().any(|f| f.id == file.id) {
                    return Err(corrupt("command.file.id", "文件 id 为空或已存在"));
                }
                Ok(Outcome::SstRegistered { file_id: file.id.clone() })
            }
            Command::AddSnapshot { snapshot } => {
                if st.snapshots.iter().any(|s| s.id == snapshot.id) {
                    return Err(corrupt("command.snapshot.id", "snapshot id 已存在"));
                }
                Ok(Outcome::SnapshotAdded { id: snapshot.id.clone() })
            }
            Command::CloseSnapshot { snapshot_id } => {
                if st.snapshots.iter().any(|s| s.id.as_str() == snapshot_id) {
                    Ok(Outcome::SnapshotClosed { id: snapshot_id.clone() })
                } else {
                    Err(corrupt("command.snapshot_id", format!("snapshot {snapshot_id} 不存在")))
                }
            }
            Command::FlushMemtable { wal_ids, level, .. } => {
                if wal_ids.is_empty() {
                    return Err(corrupt("command.wal_ids", "flush 输入为空：输入不完整"));
                }
                for wid in wal_ids {
                    let record = st.wal.iter().find(|w| &w.id == wid);
                    match record {
                        Some(r) if r.current() => {}
                        Some(_) => return Err(corrupt("command.wal_ids", format!("{wid} 已被修订取代，不能再次 flush"))),
                        None => return Err(corrupt("command.wal_ids", format!("flush 引用了不存在的 WAL {wid}"))),
                    }
                }
                let (flush, file, _flags) = build_flush(st, wal_ids, *level, command.size_hint_ref().cloned())?;
                if st.files.iter().any(|f| f.id == file.id) {
                    return Err(corrupt("command.flush", format!("flush {} 的输出文件已存在", flush.id)));
                }
                Ok(Outcome::Flushed { flush_id: flush.id, file_id: file.id })
            }
            Command::ProposeCompaction { job_name, seed_file_id } => {
                if !st.files.iter().any(|f| &f.id == seed_file_id) {
                    return Err(corrupt("command.seed_file_id", format!("种子文件 {seed_file_id} 不存在")));
                }
                let params = st.current_params()?;
                let seed = st.files.iter().find(|f| &f.id == seed_file_id).unwrap();
                let mut wal_ids: Vec<String> = Vec::new();
                let pick = crate::domain::compact::pick_inputs(&st.files, seed_file_id)?;
                for input_id in &pick.inputs {
                    if let Some(file) = st.files.iter().find(|f| &f.id == input_id) {
                        for entry in &file.entries {
                            if entry.origin.starts_with("wal_") && !wal_ids.contains(&entry.origin) {
                                wal_ids.push(entry.origin.clone());
                            }
                        }
                        for frag in &file.range_fragments {
                            if !wal_ids.contains(&frag.origin_wal) {
                                wal_ids.push(frag.origin_wal.clone());
                            }
                        }
                    }
                }
                let result = run_compaction(job_name, &st.files, &st.snapshots, params, seed_file_id, &wal_ids)?;
                if st.compactions.iter().any(|c| c.id == result.id) {
                    return Err(corrupt("command.job_name", "等价压实候选已存在（材料/参数相同）"));
                }
                Ok(Outcome::CompactionProposed { compaction: result })
            }
            Command::InterruptCompaction { compaction_id, phase } => {
                let comp = find_compaction(st, compaction_id)
                    .ok_or_else(|| corrupt("command.compaction_id", format!("压实 {compaction_id} 不存在")))?;
                match comp.status {
                    CompactionStatus::Proposed => {
                        Ok(Outcome::CompactionInterrupted { compaction_id: compaction_id.clone(), phase: phase.clone() })
                    }
                    CompactionStatus::Interrupted => {
                        Err(corrupt("command.compaction_id", "该压实已处于中断状态"))
                    }
                    CompactionStatus::Installed => Err(corrupt("command.compaction_id", "已安装的压实不能中断")),
                    CompactionStatus::Rejected => Err(corrupt("command.compaction_id", "已否定的压实不能中断")),
                }
            }
            Command::InstallCompaction { compaction_id } => {
                let comp = find_compaction(st, compaction_id)
                    .ok_or_else(|| corrupt("command.compaction_id", format!("压实 {compaction_id} 不存在")))?;
                match comp.status {
                    CompactionStatus::Installed => Ok(Outcome::DuplicateInstallRejected {
                        compaction_id: compaction_id.clone(),
                        first_installed_by: comp.installed_by.clone().unwrap_or_default(),
                    }),
                    CompactionStatus::Rejected => {
                        Err(corrupt("command.compaction_id", "已否定的压实结果不能安装"))
                    }
                    CompactionStatus::Proposed | CompactionStatus::Interrupted => Ok(Outcome::CompactionInstalled {
                        compaction_id: compaction_id.clone(),
                        outputs: comp.outputs.iter().map(|o| o.file_id.clone()).collect(),
                        removed: comp.inputs.clone(),
                    }),
                }
            }
            Command::Decide { target_id, action, note: _ } => {
                let comp = find_compaction(st, target_id)
                    .ok_or_else(|| corrupt("command.target_id", format!("决定目标 {target_id} 不存在")))?;
                let conflict = match action {
                    DecideAction::Accept => match comp.status {
                        CompactionStatus::Proposed | CompactionStatus::Interrupted => None,
                        CompactionStatus::Installed => Some("target_already_installed".to_string()),
                        CompactionStatus::Rejected => Some("target_already_rejected".to_string()),
                    },
                    DecideAction::Reject => match comp.status {
                        CompactionStatus::Proposed | CompactionStatus::Interrupted => None,
                        CompactionStatus::Installed => Some("target_already_installed".to_string()),
                        CompactionStatus::Rejected => Some("target_already_rejected".to_string()),
                    },
                    DecideAction::AdjustParams { .. } => None,
                };
                let decision_id = stable_id(
                    "dec",
                    &serde_json::json!({"target": target_id, "action": action, "parent_event": event_id}),
                );
                Ok(Outcome::DecisionRecorded { decision_id, conflict })
            }
            Command::ReviseParams { params } => {
                if st.params.iter().any(|p| p.version == params.version) {
                    return Err(corrupt("command.params.version", format!("参数版本 {} 已存在", params.version)));
                }
                Ok(Outcome::ParamsRevised { version: params.version.clone() })
            }
            Command::FixSource { record_id, new_data, .. } => {
                let old = st
                    .wal
                    .iter()
                    .find(|w| &w.id == record_id)
                    .ok_or_else(|| corrupt("command.record_id", format!("WAL 记录 {record_id} 不存在")))?;
                if !old.current() {
                    return Err(corrupt("command.record_id", "被修订记录本身已被取代"));
                }
                match (&old.data, new_data) {
                    (WalRecordData::Point(a), WalRecordData::Point(b)) if a.key == b.key && a.seq == b.seq => {}
                    (WalRecordData::RangeDelete(a), WalRecordData::RangeDelete(b)) if a.seq == b.seq => {}
                    _ => {
                        return Err(corrupt(
                            "command.new_data",
                            "源数据修订只能改变值/端点，不能改变 key 或 sequence number",
                        ))
                    }
                }
                let new = WalRecord::new(old.version + 1, old.epoch_ms, new_data.clone(), Some(old.id.clone()));
                let stale = compute_stale(st, record_id);
                Ok(Outcome::SourceFixed { new_record_id: new.id, stale })
            }
            Command::ReadProbe { probe } => {
                let snapshot_id = probe.snapshot_id();
                if !st.snapshots.iter().any(|s| s.id == snapshot_id) {
                    return Err(corrupt("command.probe.snapshot_id", format!("snapshot {snapshot_id} 不存在")));
                }
                let probe_id = stable_id(
                    "probe",
                    &serde_json::json!({"spec": probe, "parent_event": self.log.last().map(|e| e.id.clone())}),
                );
                let result = run_probe(st, probe)?;
                Ok(Outcome::ProbeResult { probe_id, result })
            }
            Command::ForkBranch { branch } => {
                if branch.is_empty() {
                    return Err(corrupt("command.branch", "分支名不能为空"));
                }
                Ok(Outcome::BranchForked { branch: branch.clone() })
            }
        }
    }
}

impl Command {
    fn size_hint_ref(&self) -> Option<&SizeHint> {
        match self {
            Command::FlushMemtable { size_hint, .. } => size_hint.as_ref(),
            _ => None,
        }
    }
}

pub(crate) fn find_compaction<'a>(st: &'a State, id: &str) -> Option<&'a CompactionResult> {
    st.compactions.iter().find(|c| c.id == id || c.job_name == id)
}

pub(crate) fn find_compaction_mut<'a>(st: &'a mut State, id: &str) -> Option<&'a mut CompactionResult> {
    st.compactions.iter_mut().find(|c| c.id == id || c.job_name == id)
}

impl ProbeSpec {
    pub fn snapshot_id(&self) -> &str {
        match self {
            ProbeSpec::PointLookup { snapshot_id, .. }
            | ProbeSpec::RangeScan { snapshot_id, .. }
            | ProbeSpec::Coverage { snapshot_id } => snapshot_id,
        }
    }
}

fn compute_stale(st: &State, changed_wal: &str) -> Vec<String> {
    let mut stale: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stale_ssts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for flush in &st.flushes {
        if flush.spec.wal_ids.iter().any(|id| id == changed_wal) {
            stale.insert(flush.id.clone());
            stale_ssts.insert(flush.output_file_id.clone());
        }
    }
    loop {
        let mut changed = false;
        for comp in &st.compactions {
            if stale.contains(&comp.id) {
                continue;
            }
            let touches_wal = comp.provenance.input_wal.iter().any(|id| id == changed_wal);
            let touches_file = comp.inputs.iter().any(|id| stale_ssts.contains(id));
            if touches_wal || touches_file {
                stale.insert(comp.id.clone());
                changed = true;
                for out in &comp.outputs {
                    stale_ssts.insert(out.file_id.clone());
                }
            }
        }
        if !changed {
            break;
        }
    }
    stale.into_iter().collect()
}

fn build_flush(
    st: &State,
    wal_ids: &[String],
    level: u32,
    size_hint: Option<SizeHint>,
) -> LoadResult<(FlushRecord, SstFile, Vec<Flag>)> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut fragments: Vec<RangeFragment> = Vec::new();
    let mut key_bounds: Vec<(String, bool)> = Vec::new(); // (key, inclusive)
    let mut flags = Vec::new();
    let mut point_index: std::collections::BTreeMap<(String, Seq), Vec<(PointKind, String)>> =
        std::collections::BTreeMap::new();

    for wid in wal_ids {
        let record = st.wal.iter().find(|w| &w.id == wid).expect("validated wal id");
        match &record.data {
            WalRecordData::Point(p) => {
                entries.push(Entry {
                    entry_id: entry_id(&record.id, &p.key, p.seq, p.op_type()),
                    key: p.key.clone(),
                    seq: p.seq,
                    op: p.op_type(),
                    value: p.value.clone(),
                    origin: record.id.clone(),
                });
                key_bounds.push((p.key.clone(), true));
                point_index
                    .entry((p.key.clone(), p.seq))
                    .or_default()
                    .push((p.kind, record.id.clone()));
            }
            WalRecordData::RangeDelete(r) => {
                fragments.push(RangeFragment {
                    fragment_id: fragment_id(&r.name, r.seq, &r.begin, &r.end_exclusive, &record.id),
                    name: r.name.clone(),
                    seq: r.seq,
                    begin: r.begin.clone(),
                    end_exclusive: r.end_exclusive.clone(),
                    origin_wal: record.id.clone(),
                    retention: vec!["flush_initial".to_string()],
                });
                key_bounds.push((r.begin.clone(), true));
                key_bounds.push((r.end_exclusive.clone(), false));
            }
        }
    }

    // 同序号多类型：保留为显式语义标记，读取探针给出全部合法候选。
    for ((key, seq), kinds) in point_index {
        let uniq: std::collections::BTreeSet<PointKind> = kinds.iter().map(|(k, _)| *k).collect();
        if uniq.len() > 1 {
            let wal_ids_found = kinds.iter().map(|(_, id)| id.clone()).collect();
            flags.push(Flag {
                id: stable_id("flag", &serde_json::json!({"kind": "same_seq_multi_type", "key": key, "seq": seq})),
                kind: "same_seq_multi_type".to_string(),
                key: key.clone(),
                seq,
                detail: format!(
                    "同一内部键 ({key}, seq={seq}) 上同时存在 PUT 与 DELETE，内部键顺序给 DELETE 优先权，但保留全部合法候选"
                ),
                wal_ids: wal_ids_found,
            });
        }
    }

    entries.sort_by(internals::internal_order);
    fragments.sort_by(|a, b| a.begin.cmp(&b.begin).then_with(|| b.seq.cmp(&a.seq)));

    let smallest = key_bounds.iter().map(|(k, _)| k.clone()).min().unwrap_or_default();
    let largest_bound = key_bounds
        .iter()
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .cloned()
        .unwrap_or((String::new(), true));
    let file_seed = serde_json::json!({"wal_ids": wal_ids, "level": level});
    let file_id = stable_id("sst", &file_seed);
    let flush_seed = serde_json::json!({"wal_ids": wal_ids, "level": level, "file": file_id});
    let flush_id = stable_id("flush", &flush_seed);

    let file = SstFile {
        id: file_id,
        level,
        smallest,
        largest: largest_bound.0,
        end_inclusive: largest_bound.1,
        entries,
        range_fragments: fragments,
        size_hint: size_hint.clone(),
        origin: flush_id.clone(),
    };
    Ok((FlushRecord { id: flush_id, spec: FlushSpec { wal_ids: wal_ids.to_vec(), level, size_hint }, output_file_id: file.id.clone() }, file, flags))
}

fn build_output_file(comp: &CompactionResult, output: &OutputSummary) -> SstFile {
    SstFile {
        id: output.file_id.clone(),
        level: output.level,
        smallest: output.smallest.clone(),
        largest: output.largest.clone(),
        end_inclusive: output.end_inclusive,
        entries: output.entries.clone(),
        range_fragments: output.range_fragments.clone(),
        size_hint: None,
        origin: comp.id.clone(),
    }
}

fn read_to_json(read: internals::PointRead) -> serde_json::Value {
    match read {
        internals::PointRead::Value { entry } => {
            serde_json::json!({"kind": "value", "entry_id": entry.entry_id, "key": entry.key, "seq": entry.seq,
                "value": entry.value.map(|v| serde_json::json!({"display": v.display(), "raw": v.raw, "raw_unit": v.raw_unit,
                    "normalized": v.normalized, "normalized_unit": v.normalized_unit, "diagnostics": v.diagnostics}))})
        }
        internals::PointRead::PointTombstone { entry } => serde_json::json!({
            "kind": "point_tombstone", "entry_id": entry.entry_id, "key": entry.key, "seq": entry.seq}),
        internals::PointRead::RangeTombstone { range, key_seq } => serde_json::json!({
            "kind": "range_tombstone", "range": range.name, "range_seq": range.seq,
            "begin": range.begin, "end_exclusive": range.end_exclusive, "key_seq": key_seq}),
        internals::PointRead::NotFound => serde_json::json!({"kind": "not_found"}),
    }
}

fn run_probe(st: &State, probe: &ProbeSpec) -> LoadResult<serde_json::Value> {
    let mut files: Vec<&SstFile> = st.files.iter().collect();
    files.sort_by(|a, b| {
        a.level
            .cmp(&b.level)
            .then_with(|| a.smallest.cmp(&b.smallest))
            .then_with(|| a.id.cmp(&b.id))
    });
    let snapshot_id = probe.snapshot_id();
    let snapshot = st.snapshots.iter().find(|s| s.id == snapshot_id).unwrap();

    match probe {
        ProbeSpec::PointLookup { key, .. } => {
            let read = internals::point_lookup(&files, key, snapshot);
            // 同序号歧义只有在该对记录真的是“最新可见版本”时才影响结论；被更新条目遮蔽时不算。
            let newest_visible_seq = st
                .files
                .iter()
                .flat_map(|f| f.entries.iter())
                .filter(|e| &e.key == key && e.seq <= snapshot.seq)
                .map(|e| e.seq)
                .max();
            let read = read;
            let mut json = read_to_json(read);
            let flag = st.flags.iter().find(|f| {
                f.kind == "same_seq_multi_type"
                    && &f.key == key
                    && f.seq <= snapshot.seq
                    && newest_visible_seq.is_some_and(|n| f.seq >= n)
            });
            if let Some(flag) = flag {
                let mut map = json.as_object().unwrap().clone();
                map.insert("kind".to_string(), serde_json::json!("ambiguous_same_seq_multi_type"));
                map.insert(
                    "candidates".to_string(),
                    serde_json::json!([
                        {"interpretation": "delete_wins_by_internal_order", "read": {"kind": "point_tombstone", "seq": flag.seq}},
                        {"interpretation": "put_survives", "read": {"kind": "value", "seq": flag.seq}}
                    ]),
                );
                map.insert("flag_id".to_string(), serde_json::json!(flag.id));
                json = serde_json::Value::Object(map);
            }
            Ok(json)
        }
        ProbeSpec::RangeScan { begin, end_exclusive, .. } => {
            let scanned = internals::range_scan(&files, begin, end_exclusive, snapshot);
            let rows: Vec<serde_json::Value> = scanned
                .into_iter()
                .map(|row| {
                    let mut json = serde_json::json!({"key": row.key, "read": read_to_json(row.read)});
                    let newest_seq = st
                        .files
                        .iter()
                        .flat_map(|f| f.entries.iter())
                        .filter(|e| e.key == row.key && e.seq <= snapshot.seq)
                        .map(|e| e.seq)
                        .max();
                    if let Some(flag) = st.flags.iter().find(|f| {
                        f.kind == "same_seq_multi_type"
                            && f.key == row.key
                            && f.seq <= snapshot.seq
                            && newest_seq.is_some_and(|n| f.seq >= n)
                    }) {
                        json["ambiguous"] = serde_json::json!({
                            "flag_id": flag.id,
                            "candidates": ["delete_wins_by_internal_order", "put_survives"]
                        });
                    }
                    json
                })
                .collect();
            Ok(serde_json::json!({"begin": begin, "end_exclusive": end_exclusive,
                "snapshot": snapshot.id, "snapshot_seq": snapshot.seq, "rows": rows}))
        }
        ProbeSpec::Coverage { .. } => {
            let reports = internals::tombstone_coverage(&files, snapshot);
            Ok(serde_json::json!({
                "snapshot": snapshot.id, "snapshot_seq": snapshot.seq,
                "reports": reports.into_iter().map(|r| serde_json::json!({
                    "fragment_id": r.fragment_id, "name": r.name, "seq": r.seq,
                    "begin": r.begin, "end_exclusive": r.end_exclusive, "visible": r.visible,
                    "covered_point_entries": r.covered_point_entries
                })).collect::<Vec<_>>()
            }))
        }
    }
}

fn mutate(st: &mut State, event: &Event) {
    match (&event.command, &event.outcome) {
        (Command::ImportWal { records }, Outcome::WalImported { .. }) => {
            st.wal.extend(records.iter().cloned());
        }
        (Command::RegisterSst { file }, Outcome::SstRegistered { .. }) => {
            st.files.push(file.clone());
        }
        (Command::AddSnapshot { snapshot }, Outcome::SnapshotAdded { .. }) => {
            st.snapshots.push(snapshot.clone());
        }
        (Command::CloseSnapshot { snapshot_id }, Outcome::SnapshotClosed { .. }) => {
            if let Some(snapshot) = st.snapshots.iter_mut().find(|s| &s.id == snapshot_id) {
                snapshot.active = false;
            }
        }
        (Command::FlushMemtable { wal_ids, level, size_hint }, Outcome::Flushed { .. }) => {
            let (flush, file, flags) =
                build_flush(st, wal_ids, *level, size_hint.clone()).expect("validated flush");
            st.flushes.push(flush);
            st.files.push(file);
            for flag in flags {
                if !st.flags.iter().any(|f| f.id == flag.id) {
                    st.flags.push(flag);
                }
            }
        }
        (Command::ProposeCompaction { .. }, Outcome::CompactionProposed { compaction }) => {
            st.compactions.push(compaction.clone());
        }
        (Command::InterruptCompaction { compaction_id, phase }, Outcome::CompactionInterrupted { .. }) => {
            if let Some(comp) = st.compactions.iter_mut().find(|c| &c.id == compaction_id) {
                comp.status = CompactionStatus::Interrupted;
                comp.diagnostics.push(format!("压实中断于阶段 {phase}；候选输出保留，可恢复安装或否定"));
            }
        }
        (Command::InstallCompaction { compaction_id }, Outcome::CompactionInstalled { outputs, .. }) => {
            let install_plan = find_compaction(st, compaction_id).map(|comp| {
                let input_ids: std::collections::BTreeSet<String> = comp.inputs.iter().cloned().collect();
                let out_files: Vec<SstFile> = comp
                    .outputs
                    .iter()
                    .map(|o| build_output_file(comp, o))
                    .collect();
                (input_ids, out_files)
            });
            if let Some((input_ids, out_files)) = install_plan {
                st.files.retain(|f| !input_ids.contains(&f.id));
                for file in out_files {
                    if !st.files.iter().any(|f| f.id == file.id) {
                        st.files.push(file);
                    }
                }
                if let Some(comp) = find_compaction_mut(st, compaction_id) {
                    comp.status = CompactionStatus::Installed;
                    comp.installed_by = Some(event.id.clone());
                }
            }
            let _ = outputs;
        }
        (Command::InstallCompaction { .. }, Outcome::DuplicateInstallRejected { .. }) => {}
        (Command::Decide { target_id, action, note }, Outcome::DecisionRecorded { decision_id, conflict }) => {
            enum Plan {
                Install,
                Reject,
                Adjust { params: Params, comp_id: String },
                None,
            }
            let plan = if let Some(comp) = find_compaction(st, target_id) {
                match action {
                    DecideAction::Accept => {
                        if matches!(comp.status, CompactionStatus::Proposed | CompactionStatus::Interrupted) {
                            Plan::Install
                        } else {
                            Plan::None
                        }
                    }
                    DecideAction::Reject => {
                        if matches!(comp.status, CompactionStatus::Proposed | CompactionStatus::Interrupted) {
                            Plan::Reject
                        } else {
                            Plan::None
                        }
                    }
                    DecideAction::AdjustParams { params } => {
                        Plan::Adjust { params: params.clone(), comp_id: comp.id.clone() }
                    }
                }
            } else {
                Plan::None
            };

            match plan {
                Plan::Install => {
                    let install = find_compaction(st, target_id).map(|comp| {
                        let input_ids: std::collections::BTreeSet<String> = comp.inputs.iter().cloned().collect();
                        let out_files: Vec<SstFile> = comp
                            .outputs
                            .iter()
                            .map(|o| build_output_file(comp, o))
                            .collect();
                        (input_ids, out_files)
                    });
                    if let Some((input_ids, out_files)) = install {
                        st.files.retain(|f| !input_ids.contains(&f.id));
                        for file in out_files {
                            if !st.files.iter().any(|f| f.id == file.id) {
                                st.files.push(file);
                            }
                        }
                        if let Some(comp) = find_compaction_mut(st, target_id) {
                            comp.status = CompactionStatus::Installed;
                            comp.installed_by = Some(event.id.clone());
                        }
                    }
                }
                Plan::Reject => {
                    if let Some(comp) = find_compaction_mut(st, target_id) {
                        comp.status = CompactionStatus::Rejected;
                    }
                }
                Plan::Adjust { params, comp_id } => {
                    if !st.params.iter().any(|p| p.version == params.version) {
                        st.params.push(params.clone());
                    }
                    if !st.stale.iter().any(|m| m.object_id == comp_id) {
                        st.stale.push(StaleMark {
                            object_id: comp_id,
                            object_kind: "compaction".to_string(),
                            reason: format!("参数被调整为 {}，该候选基于旧参数", params.version),
                            marked_by_event: event.id.clone(),
                        });
                    }
                }
                Plan::None => {}
            }

            st.decisions.push(Decision {
                id: decision_id.clone(),
                target_id: target_id.clone(),
                action: match action {
                    DecideAction::Accept => "accept".to_string(),
                    DecideAction::Reject => "reject".to_string(),
                    DecideAction::AdjustParams { .. } => "adjust_params".to_string(),
                },
                note: note.clone(),
                conflict: conflict.clone(),
                event_id: event.id.clone(),
                ordinal: event.ordinal,
            });
        }

        (Command::ReviseParams { params }, Outcome::ParamsRevised { .. }) => {
            st.params.push(params.clone());
        }
        (Command::FixSource { record_id, new_data, epoch_ms }, Outcome::SourceFixed { stale: stale_ids, .. }) => {
            let old = st.wal.iter().find(|w| &w.id == record_id).expect("validated record").clone();
            let new = WalRecord::new(old.version + 1, *epoch_ms, new_data.clone(), Some(old.id.clone()));
            let new_id = new.id.clone();
            if let Some(old) = st.wal.iter_mut().find(|w| &w.id == record_id) {
                old.superseded_by = Some(new_id.clone());
            }
            st.wal.push(new);
            for object_id in stale_ids {
                let kind = if st.flushes.iter().any(|f| &f.id == object_id) {
                    "flush"
                } else {
                    "compaction"
                };
                if !st.stale.iter().any(|m| m.object_id == *object_id) {
                    st.stale.push(StaleMark {
                        object_id: object_id.clone(),
                        object_kind: kind.to_string(),
                        reason: format!("直接来源 {record_id} 被修订（只影响局部依赖）"),
                        marked_by_event: event.id.clone(),
                    });
                }
            }
        }
        (Command::ReadProbe { .. }, Outcome::ProbeResult { .. }) => {}
        (Command::ForkBranch { .. }, Outcome::BranchForked { .. }) => {}
        (_, Outcome::Error { .. }) => {}
        other => panic!("命令与结果不匹配（重放损坏）：{other:?}"),
    }
}
