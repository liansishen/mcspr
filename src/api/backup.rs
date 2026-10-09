//! 实例备份接口

use crate::error::{ApiError, ApiResult};
use crate::instance::{backup, get_instance};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::Json;
use serde_json::json;

pub async fn list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let _ = get_instance(&state, &id).await?;
    let dir = crate::instance::backup::backups_dir(&state, &id).await;
    Ok(Json(json!({ "backups": backup::list(&dir) })))
}

/// Content-Disposition 头：清洗文件名中的控制字符与引号，非法时回退通用值
pub fn safe_disposition(name: &str) -> axum::http::HeaderValue {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{clean}\""))
        .unwrap_or(axum::http::HeaderValue::from_static("attachment"))
}

pub async fn create(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if !state.acquire_busy(&id) {
        return Err(ApiError::bad_request("该实例有整体操作（备份/更新/重装/克隆）正在进行，请稍候"));
    }
    let rt = match get_instance(&state, &id).await {
        Ok(rt) => rt,
        Err(e) => { state.release_busy(&id); return Err(e); }
    };
    let iname = rt.meta.read().await.name.clone();
    let result = backup::create(&state, &rt).await;
    state.release_busy(&id);
    let name = match result {
        Ok(n) => n,
        Err(e) => {
            crate::alerts::send(&state, &format!("backup-{id}"), format!("实例「{iname}」备份失败: {e}")).await;
            return Err(ApiError::bad_request(e));
        }
    };
    Ok(Json(json!({ "ok": true, "name": name })))
}

pub async fn download(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let _ = get_instance(&state, &id).await?;
    let bdir = crate::instance::backup::backups_dir(&state, &id).await;
    let path = backup::backup_file(&bdir, &name).map_err(ApiError::bad_request)?;
    let file = tokio::fs::File::open(path).await?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut hm = axum::http::HeaderMap::new();
    hm.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/gzip"),
    );
    hm.insert(
        axum::http::header::CONTENT_DISPOSITION,
        safe_disposition(&name),
    );
    let body = axum::body::Body::from_stream(stream);
    let mut resp = Response::new(body);
    *resp.headers_mut() = hm;
    Ok(resp)
}

pub async fn preview(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let bdir = crate::instance::backup::backups_dir(&state, &id).await;
    let p = backup::preview(&bdir, &rt.dir, &name).map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "preview": p })))
}

pub async fn restore(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err(ApiError::bad_request("恢复备份前请先停止实例"));
    }
    if !state.acquire_busy(&id) {
        return Err(ApiError::bad_request("该实例有整体操作正在进行，请稍候"));
    }
    let bdir = crate::instance::backup::backups_dir(&state, &id).await;
    let p = match backup::backup_file(&bdir, &name) {
        Ok(p) => p,
        Err(e) => { state.release_busy(&id); return Err(ApiError::bad_request(e)); }
    };
    let dir = rt.dir.clone();
    let r = tokio::task::spawn_blocking(move || backup::restore(&dir, &p))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))
        .and_then(|r| r.map_err(ApiError::bad_request));
    state.release_busy(&id);
    r?;
    // 恢复后实例文件可能回退，后台重新协调白名单
    super::whitelist_sync::trigger_force();
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _ = get_instance(&state, &id).await?;
    let bdir = crate::instance::backup::backups_dir(&state, &id).await;
    backup::delete(&bdir, &name).map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "ok": true })))
}
