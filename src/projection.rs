//! 投影：把事件流回放成审阅状态。
//! 回到任意旧事件时恢复当时的依赖版本和可见结论，而不是拿最新结果覆盖历史。

use crate::domain::*;
use crate::engine::CompactionOutput;
use crate::store::{Event, EventPayload, Store, StoreError};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct ReviewState {
    pub levels: Vec<LevelView>,
    pub snapshots: Vec<Snapshot>,
    pub params: CompactionParams,
    pub param_history: Vec<(u32, String)>,
    /// key -> 版本链（内部键升序即最新在前：同 key 时 seq 降序、kind 降序）
    pub version_chains: BTreeMap<String, Vec<Record>>,
    /// tombstone 覆盖关系
    pub tombstone_coverage: Vec<TombstoneCover>,
    pub compactions: Vec<CompactionRecord>,
    pub decisions: Vec<DecisionRecord>,
    pub interruptions: Vec<InterruptionRecord>,
    /// 回放到的最大事件序号（时间旅行用）
    pub replayed_until: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TombstoneCover {
    pub tombstone: String,
    pub tombstone_event: String,
    pub covered_keys: Vec<String>,
    pub retained: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionRecord {
    pub event_id: String,
    pub input_digest: String,
    pub params_version: u32,
    pub input_files: Vec<String>,
    pub reason: String,
    pub output: Option<CompactionOutput>,
    pub error: Option<String>,
    pub parent_events: Vec<String>,
    pub output_boundaries: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionRecord {
    pub event_id: String,
    pub action: String,
    pub target_event: String,
    pub rationale: String,
    pub adopted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterruptionRecord {
    pub event_id: String,
    pub input_digest: String,
    pub processed: usize,
    pub reason: String,
}

pub fn project(events: &[Event], until_seq: Option<u64>) -> ReviewState {
    let mut levels: BTreeMap<u32, Vec<SstFile>> = BTreeMap::new();
    let mut snapshots: Vec<Snapshot> = Vec::new();
    let mut params = CompactionParams::default();
    let mut param_history: Vec<(u32, String)> = Vec::new();
    let mut compactions: Vec<CompactionRecord> = Vec::new();
    let mut decisions: Vec<DecisionRecord> = Vec::new();
    let mut interruptions: Vec<InterruptionRecord> = Vec::new();
    let mut material_events: Vec<(String, Vec<SstFile>)> = Vec::new();
    let mut replayed_until = 0u64;

    for ev in events {
        if let Some(u) = until_seq {
            if ev.seq > u {
                break;
            }
        }
        replayed_until = ev.seq;
        match &ev.payload {
            EventPayload::MaterialImported { files, snapshots: snaps, .. } => {
                for f in files {
                    levels.entry(f.level).or_default().push(f.clone());
                }
                for s in snaps {
                    if let Some(existing) = snapshots.iter_mut().find(|x| x.id == s.id) {
                        *existing = s.clone();
                    } else {
                        snapshots.push(s.clone());
                    }
                }
                material_events.push((ev.id.clone(), files.clone()));
            }
            EventPayload::ParamsRevised { params: p, note } => {
                params = p.clone();
                param_history.push((p.version, note.clone()));
            }
            EventPayload::SourceAmended { file, replaces, .. } => {
                for files in levels.values_mut() {
                    files.retain(|f| f.id != *replaces);
                }
                levels.entry(file.level).or_default().push(file.clone());
            }
            EventPayload::CompactionComputed { input, output, error, parent_events, reason } => {
                let boundaries = output
                    .as_ref()
                    .map(|o| o.files.iter().map(|f| (f.id.clone(), f.smallest.clone(), f.largest.clone())).collect())
                    .unwrap_or_default();
                compactions.push(CompactionRecord {
                    event_id: ev.id.clone(),
                    input_digest: output.as_ref().map(|o| o.input_digest.clone()).unwrap_or_default(),
                    params_version: input.params.version,
                    input_files: input.input_files.iter().map(|f| f.id.clone()).collect(),
                    reason: reason.clone(),
                    output: output.clone(),
                    error: error.clone(),
                    parent_events: parent_events.clone(),
                    output_boundaries: boundaries,
                });
            }
            EventPayload::DecisionRecorded { action, target_event, rationale, adopted } => {
                decisions.push(DecisionRecord {
                    event_id: ev.id.clone(),
                    action: action.clone(),
                    target_event: target_event.clone(),
                    rationale: rationale.clone(),
                    adopted: *adopted,
                });
            }
            EventPayload::CompactionInterrupted { input_digest, processed, reason } => {
                interruptions.push(InterruptionRecord {
                    event_id: ev.id.clone(),
                    input_digest: input_digest.clone(),
                    processed: *processed,
                    reason: reason.clone(),
                });
            }
            EventPayload::ResultInstalled { files, .. } => {
                for f in files {
                    // 重复安装幂等：同 id 文件不重复入层
                    let entry = levels.entry(f.level).or_default();
                    if !entry.iter().any(|x| x.id == f.id) {
                        entry.push(f.clone());
                    }
                }
            }
        }
    }

    // 版本链与 tombstone 覆盖
    let mut version_chains: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    let mut all_records: Vec<(String, Record)> = Vec::new();
    for files in levels.values() {
        for f in files {
            for r in &f.records {
                all_records.push((f.id.clone(), r.clone()));
                if !r.is_range_del() {
                    version_chains.entry(r.ikey.user_key.clone()).or_default().push(r.clone());
                }
            }
        }
    }
    for chain in version_chains.values_mut() {
        chain.sort_by(|a, b| a.ikey.cmp(&b.ikey)); // 内部键序即最新在前
    }
    let mut tombstone_coverage = Vec::new();
    for (fid, r) in &all_records {
        if r.ikey.kind == KIND_DELETE || r.ikey.kind == KIND_RANGE_DELETE {
            let covered: Vec<String> = version_chains
                .iter()
                .filter(|(k, chain)| {
                    (r.is_range_del() && r.covers(k) || r.ikey.user_key == **k)
                        && chain.iter().any(|c| c.ikey.seq < r.ikey.seq)
                })
                .map(|(k, _)| k.clone())
                .collect();
            tombstone_coverage.push(TombstoneCover {
                tombstone: format!("{} @{}", r.ikey, fid),
                tombstone_event: material_events.first().map(|(id, _)| id.clone()).unwrap_or_default(),
                covered_keys: covered,
                retained: true,
                reason: if r.is_range_del() { "范围删除，覆盖区间内旧版本".into() } else { "point tombstone，遮蔽同 key 旧版本".into() },
            });
        }
    }

    let levels_vec: Vec<LevelView> = levels
        .into_iter()
        .map(|(level, mut files)| {
            files.sort_by(|a, b| a.id.cmp(&b.id));
            LevelView { level, files }
        })
        .collect();

    ReviewState {
        levels: levels_vec,
        snapshots,
        params,
        param_history,
        version_chains,
        tombstone_coverage,
        compactions,
        decisions,
        interruptions,
        replayed_until,
    }
}

pub fn project_store(store: &Store, branch: Option<&str>, until_seq: Option<u64>) -> Result<ReviewState, StoreError> {
    let events = store.events(branch)?;
    Ok(project(&events, until_seq))
}
