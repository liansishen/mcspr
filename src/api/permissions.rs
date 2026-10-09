//! 实例用户权限：按实例授予/撤销普通用户的查看权限。
//!
//! 单次保存原子修改相关账户，只改动目标实例的授权并保留其余实例授权；
//! 使用实例权限修订号处理多管理员并发编辑冲突。

use crate::auth::Role;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// 读取实例 `server.properties` 中的 `white-list` 开关（仅用于展示）。
async fn whitelist_enabled(state: &AppState, id: &str) -> bool {
    let dir = {
        state
            .instances
            .read()
            .await
            .get(id)
            .map(|rt| rt.dir.clone())
    };
    let Some(dir) = dir else {
        return false;
    };
    let pf = crate::instance::properties::read(&dir);
    pf.entries
        .iter()
        .find(|e| e.key.as_deref() == Some("white-list"))
        .map(|e| e.value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn build_view(state: &AppState, id: &str) -> Value {
    let revision = state.auth.instance_permission_revision(id).await;
    let users = state.auth.list().await;
    let list: Vec<Value> = users
        .iter()
        .map(|u| {
            json!({
                "id": u.id,
                "username": u.username,
                "minecraft_name": u.minecraft_name,
                "role": u.role,
                "enabled": u.enabled,
                "status": u.status,
                "granted": u.role == Role::Admin || u.instance_ids.iter().any(|i| i == id),
            })
        })
        .collect();
    json!({
        "revision": revision,
        "users": list,
        "whitelist_enabled": whitelist_enabled(state, id).await,
    })
}

pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !state.instances.read().await.contains_key(&id) {
        return super::api_error(StatusCode::NOT_FOUND, "实例不存在");
    }
    Json(build_view(&state, &id).await).into_response()
}

#[derive(Deserialize)]
pub struct PutPermissions {
    pub revision: u64,
    #[serde(default)]
    pub user_ids: Vec<String>,
}

pub async fn put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PutPermissions>,
) -> Response {
    if !state.instances.read().await.contains_key(&id) {
        return super::api_error(StatusCode::NOT_FOUND, "实例不存在");
    }
    match state
        .auth
        .set_instance_grants_checked(&id, req.revision, req.user_ids)
        .await
    {
        Ok(_) => Json(build_view(&state, &id).await).into_response(),
        Err(e) => {
            let status = if e.contains("已被其他管理员修改") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            super::api_error(status, e)
        }
    }
}
