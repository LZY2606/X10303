//! Axum 服务端：审阅页 + JSON API。
//! 业务规则在服务端与存储层可直接测试调用，不藏在浏览器中。

use crate::casepack;
use crate::engine::{compact, visible_at, CompactionInput};
use crate::projection::{project_store, ReviewState};
use crate::store::{EventPayload, Store};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

pub struct AppState {
    pub store: Store,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/events/{id}", get(api_event))
        .route("/api/visible", get(api_visible))
        .route("/api/compact", post(api_compact))
        .route("/api/decision", post(api_decision))
        .route("/api/export", get(api_export))
        .route("/api/import", post(api_import))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

#[derive(Deserialize)]
struct StateQuery {
    branch: Option<String>,
    until: Option<u64>,
}

async fn api_state(State(s): State<Arc<AppState>>, Query(q): Query<StateQuery>) -> Result<Json<ReviewState>, ApiErr> {
    let st = project_store(&s.store, q.branch.as_deref(), q.until).map_err(internal)?;
    Ok(Json(st))
}

async fn api_events(State(s): State<Arc<AppState>>, Query(q): Query<StateQuery>) -> Result<impl IntoResponse, ApiErr> {
    let evs = s.store.events(q.branch.as_deref()).map_err(internal)?;
    Ok(Json(evs))
}

async fn api_event(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    match s.store.get_event(&id).map_err(internal)? {
        Some(ev) => Ok(Json(ev)),
        None => Err(ApiErr(StatusCode::NOT_FOUND, "事件不存在".into())),
    }
}

#[derive(Deserialize)]
struct VisibleQuery {
    snap: u64,
    branch: Option<String>,
}

async fn api_visible(State(s): State<Arc<AppState>>, Query(q): Query<VisibleQuery>) -> Result<impl IntoResponse, ApiErr> {
    let st = project_store(&s.store, q.branch.as_deref(), None).map_err(internal)?;
    let files: Vec<_> = st.levels.iter().flat_map(|l| l.files.clone()).collect();
    Ok(Json(visible_at(&files, q.snap)))
}

async fn api_compact(State(s): State<Arc<AppState>>, Json(input): Json<CompactionInput>) -> Result<impl IntoResponse, ApiErr> {
    match compact(&input) {
        Ok(output) => {
            let ev = s
                .store
                .append(
                    casepack::BRANCH_MAIN,
                    EventPayload::CompactionComputed {
                        input,
                        output: Some(output.clone()),
                        error: None,
                        parent_events: vec![],
                        reason: "API 触发的压实".into(),
                    },
                )
                .map_err(internal)?;
            Ok(Json(serde_json::json!({ "ok": true, "event": ev.id, "output": output })))
        }
        Err(e) => Ok(Json(serde_json::json!({ "ok": false, "error": e }))),
    }
}

#[derive(Deserialize)]
struct DecisionReq {
    action: String,
    target_event: String,
    rationale: String,
    adopted: bool,
    branch: Option<String>,
}

async fn api_decision(State(s): State<Arc<AppState>>, Json(req): Json<DecisionReq>) -> Result<impl IntoResponse, ApiErr> {
    let branch = req.branch.clone().unwrap_or_else(|| casepack::BRANCH_MAIN.into());
    let ev = s
        .store
        .append(
            &branch,
            EventPayload::DecisionRecorded {
                action: req.action,
                target_event: req.target_event,
                rationale: req.rationale,
                adopted: req.adopted,
            },
        )
        .map_err(internal)?;
    Ok(Json(ev))
}

async fn api_export(State(s): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiErr> {
    let data = s.store.export().map_err(internal)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        data,
    ))
}

async fn api_import(State(s): State<Arc<AppState>>, body: String) -> Result<impl IntoResponse, ApiErr> {
    match s.store.import(&body) {
        Ok(n) => Ok(Json(serde_json::json!({ "ok": true, "imported": n }))),
        Err(e) => Err(ApiErr(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())),
    }
}

fn internal(e: crate::store::StoreError) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub struct ApiErr(StatusCode, String);
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
