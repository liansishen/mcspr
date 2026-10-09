//! 实例白名单同步状态与重试接口（路由由 `api/mod.rs` 接入）。
//!
//! - `GET  /instances/{id}/whitelist-sync`       → [`get`]
//! - `POST /instances/{id}/whitelist-sync/retry` → [`retry`]

use crate::error::ApiResult;
use crate::instance::whitelist_sync::{self, SyncStatus};
use crate::instance::get_instance;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

/// `GET /instances/{id}/whitelist-sync`：返回来源登记、待重试成员与最近错误。
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    Ok(Json(whitelist_sync::status_view(&rt).await))
}

/// `POST /instances/{id}/whitelist-sync/retry`：用已持久化的期望集合重新协调。
pub async fn retry(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    let outcome = whitelist_sync::retry_instance(&state, &rt).await;
    let ok = matches!(outcome.status, SyncStatus::Applied | SyncStatus::Idle);
    Ok(Json(json!({ "ok": ok, "outcome": outcome })))
}
