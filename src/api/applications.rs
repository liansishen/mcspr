//! 管理员审批接口：注册申请与游戏名变更申请的列表、批准、拒绝。
//!
//! 批准/拒绝均要求申请修订号匹配，避免批准正在被修改的旧资料；批准注册申请时
//! 在同一账户持久化事务中分配实例授权。

use crate::auth::Identity;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

pub async fn list(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "applications": state.auth.applications().await }))
}

#[derive(Deserialize)]
pub struct ApproveReq {
    pub revision: u64,
    #[serde(default)]
    pub instance_ids: Vec<String>,
}

pub async fn approve(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Json(req): Json<ApproveReq>,
) -> Response {
    if let Err(e) = super::accounts::validate_instance_ids(&state, &req.instance_ids).await {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    match state
        .auth
        .approve_application(&id, req.revision, req.instance_ids, &identity.username)
        .await
    {
        Ok(()) => {
            super::whitelist_sync::trigger();
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => apply_error(e),
    }
}

#[derive(Deserialize)]
pub struct RejectReq {
    pub revision: u64,
    #[serde(default)]
    pub reason: String,
}

pub async fn reject(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Json(req): Json<RejectReq>,
) -> Response {
    match state
        .auth
        .reject_application(&id, req.revision, &req.reason, &identity.username)
        .await
    {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => apply_error(e),
    }
}

/// 将申请操作错误映射为状态码：不存在 -> 404，修订冲突 -> 409，其余 -> 400。
fn apply_error(e: String) -> Response {
    let status = if e.contains("不存在") {
        StatusCode::NOT_FOUND
    } else if e.contains("已被修改") || e.contains("状态已变化") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    super::api_error(status, e)
}
