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

pub async fn create(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let iname = rt.meta.read().await.name.clone();
    let name = match backup::create(&state, &rt).await {
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
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .unwrap(),
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
    let bdir = crate::instance::backup::backups_dir(&state, &id).await;
    let p = backup::backup_file(&bdir, &name).map_err(ApiError::bad_request)?;
    let dir = rt.dir.clone();
    tokio::task::spawn_blocking(move || backup::restore(&dir, &p))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
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
