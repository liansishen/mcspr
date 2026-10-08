//! 面板账户管理接口（全部要求管理员身份，由路由中间件统一校验）。

use crate::auth::Role;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct CreateAccount {
    pub username: String,
    pub password: String,
    pub role: String,
    #[serde(default)]
    pub instance_ids: Vec<String>,
}

pub async fn list(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "accounts": state.auth.list().await }))
}

pub async fn create(State(state): State<AppState>, Json(req): Json<CreateAccount>) -> Response {
    let Some(role) = Role::parse(&req.role) else {
        return super::api_error(StatusCode::BAD_REQUEST, "role 仅支持 admin / user");
    };
    if let Err(e) = validate_instance_ids(&state, &req.instance_ids).await {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    match state
        .auth
        .create_user(&req.username, &req.password, role, req.instance_ids)
        .await
    {
        Ok(user) => (StatusCode::CREATED, Json(json!({ "account": user }))).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
pub struct PatchAccount {
    pub enabled: Option<bool>,
    pub role: Option<String>,
}

pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PatchAccount>,
) -> Response {
    let role = match req.role.as_deref() {
        Some(r) => match Role::parse(r) {
            Some(role) => Some(role),
            None => {
                return super::api_error(StatusCode::BAD_REQUEST, "role 仅支持 admin / user");
            }
        },
        None => None,
    };
    // 角色与启用状态在同一次原子写入中应用，并与最后管理员校验保持一致
    match state.auth.update_user(&id, req.enabled, role).await {
        Ok(user) => Json(json!({ "account": user })).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.auth.delete(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
pub struct ResetPassword {
    pub password: String,
}

pub async fn reset_password(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ResetPassword>,
) -> Response {
    match state.auth.reset_password(&id, &req.password).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
pub struct SetInstances {
    pub instance_ids: Vec<String>,
}

pub async fn set_instances(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetInstances>,
) -> Response {
    if let Err(e) = validate_instance_ids(&state, &req.instance_ids).await {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    match state.auth.set_instances(&id, req.instance_ids).await {
        Ok(()) => match state.auth.get(&id).await {
            Some(user) => Json(json!({ "account": user })).into_response(),
            None => super::api_error(StatusCode::NOT_FOUND, "账户不存在"),
        },
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

/// 授权保存前校验实例存在，避免写入无效实例 ID。
async fn validate_instance_ids(state: &AppState, ids: &[String]) -> Result<(), String> {
    let map = state.instances.read().await;
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            return Err("实例 ID 不能为空".to_string());
        }
        if !map.contains_key(id) {
            return Err(format!("实例不存在: {id}"));
        }
    }
    Ok(())
}
