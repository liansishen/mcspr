mod backup;
mod console;
mod instances;
mod resources;

use crate::config;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

pub fn router(state: AppState) -> Router {
    // ping 无需鉴权，前端用它获取版本并判断是否启用了令牌
    let ping = Router::new()
        .route("/api/ping", get(ping))
        .with_state(state.clone());

    let api = Router::new()
        .route("/stats", get(instances::stats))
        .route("/instances", get(instances::list).post(instances::create))
        .route("/versions", get(instances::versions))
        .route(
            "/loaders/{loader}/game-versions",
            get(instances::loader_game_versions),
        )
        .route(
            "/loaders/{loader}/loader-versions",
            get(instances::loader_versions),
        )
        .route("/moddb/search", get(resources::moddb_search))
        .route("/moddb/versions", get(resources::moddb_versions))
        .route("/moddb/projects", get(resources::moddb_projects))
        .route("/moddb/version-files", post(resources::moddb_version_files))
        .route("/instances/import/upload", post(instances::import_upload))
        .route("/instances/import/path", post(instances::import_path))
        .route("/jobs/{id}", get(instances::get_job))
        .route(
            "/instances/{id}",
            get(instances::detail).patch(instances::update).delete(instances::remove),
        )
        .route("/instances/{id}/start", post(instances::start))
        .route("/instances/{id}/stop", post(instances::stop))
        .route("/instances/{id}/restart", post(instances::restart))
        .route("/instances/{id}/command", post(instances::command))
        .route("/instances/{id}/status", get(instances::status))
        .route("/instances/{id}/eula", post(instances::accept_eula))
        .route("/instances/{id}/open", post(instances::open_folder))
        .route("/instances/{id}/console", get(console::console))
        .route("/instances/{id}/ws", get(console::ws))
        .route("/instances/{id}/users", get(resources::users_get))
        .route("/instances/{id}/users/action", post(resources::users_action))
        .route("/instances/{id}/mods", get(resources::mods_list))
        .route("/instances/{id}/mods/toggle", post(resources::mods_toggle))
        .route("/instances/{id}/mods/delete", post(resources::mods_delete))
        .route("/instances/{id}/mods/upload", post(resources::mods_upload))
        .route("/instances/{id}/mods/hashes", get(resources::mods_hashes))
        .route(
            "/instances/{id}/mods/download",
            post(resources::mods_download),
        )
        .route("/instances/{id}/files", get(resources::files_list))
        .route(
            "/instances/{id}/files/content",
            get(resources::file_get).put(resources::file_put),
        )
        .route("/instances/{id}/files/mkdir", post(resources::files_mkdir))
        .route("/instances/{id}/files/delete", post(resources::files_delete))
        .route("/instances/{id}/files/rename", post(resources::files_rename))
        .route("/instances/{id}/files/upload", post(resources::files_upload))
        .route("/instances/{id}/jars", get(resources::jars))
        .route(
            "/instances/{id}/properties",
            get(resources::props_get).put(resources::props_put),
        )
        .route("/settings", get(get_settings).put(put_settings))
        .route("/javas", get(javas))
        .route("/javas/scan", post(javas_scan))
        .route("/java-install/list", get(java_install_list))
        .route("/java-install/{major}", post(java_install))
        .route(
            "/instances/{id}/backups",
            get(backup::list).post(backup::create),
        )
        .route(
            "/instances/{id}/backups/{name}",
            get(backup::download).delete(backup::delete),
        )
        .route(
            "/instances/{id}/backups/{name}/preview",
            get(backup::preview),
        )
        .route(
            "/instances/{id}/backups/{name}/restore",
            post(backup::restore),
        )
        .route("/java", get(java_version))
        .route("/audit", get(audit_query))
        .route(
            "/instances/{id}/metrics",
            get(resources::metrics),
        )
        .route(
            "/instances/{id}/crashes",
            get(resources::crashes_list),
        )
        .route(
            "/instances/{id}/crashes/file",
            get(resources::crashes_file),
        )
        .route(
            "/instances/{id}/playtime",
            get(resources::playtime),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth_mw))
        .layer(middleware::from_fn_with_state(state.clone(), audit_mw))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state);

    Router::new()
        .merge(ping)
        .nest("/api", api)
        .fallback(static_handler)
}

async fn auth_mw(state: State<AppState>, req: Request, next: Next) -> Response {
    let token = state.config.read().await.token.clone();
    if !token.is_empty() {
        let header_ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == format!("Bearer {token}"))
            .unwrap_or(false);
        let query_ok = req.uri().query().map(|q| {
            q.split('&').any(|pair| {
                pair.split_once('=')
                    .map(|(k, v)| k == "token" && v == token)
                    .unwrap_or(false)
            })
        }).unwrap_or(false);
        if !header_ok && !query_ok {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "需要访问令牌"}))).into_response();
        }
    }
    next.run(req).await
}

async fn ping(State(state): State<AppState>) -> Json<serde_json::Value> {
    let auth_required = !state.config.read().await.token.is_empty();
    Json(json!({
        "ok": true,
        "name": "MCS Panel",
        "version": env!("CARGO_PKG_VERSION"),
        "auth_required": auth_required,
    }))
}

#[derive(Deserialize)]
struct SettingsUpdate {
    listen: Option<String>,
    token: Option<String>,
    data_dir: Option<String>,
    curseforge_api_key: Option<String>,
}

async fn get_settings(State(state): State<AppState>) -> Json<config::PanelConfig> {
    Json(state.config.read().await.clone())
}

async fn put_settings(
    State(state): State<AppState>,
    Json(u): Json<SettingsUpdate>,
) -> ApiResult<Json<serde_json::Value>> {
    {
        let mut cfg = state.config.write().await;
        if let Some(l) = u.listen {
            if !l.trim().is_empty() {
                cfg.listen = l.trim().to_string();
            }
        }
        if let Some(t) = u.token {
            cfg.token = t.trim().to_string();
        }
        if let Some(d) = u.data_dir {
            if !d.trim().is_empty() {
                cfg.data_dir = d.trim().to_string();
            }
        }
        if let Some(k) = u.curseforge_api_key {
            cfg.curseforge_api_key = k.trim().to_string();
        }
        config::save(&cfg)?;
    }
    Ok(Json(json!({
        "ok": true,
        "note": "listen / data_dir 需重启面板后生效；token 立即生效"
    })))
}

/// 一键安装的 Java 运行时列表
async fn java_install_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "installed": crate::instance::javainstall::load_registry(&state).await }))
}

/// 一键安装指定大版本的 Temurin JRE（后台任务）
async fn java_install(State(state): State<AppState>, Path(major): Path<u32>) -> ApiResult<Json<serde_json::Value>> {
    if !crate::instance::javainstall::MAJORS.contains(&major) {
        return Err(ApiError::bad_request(format!("仅支持安装以下大版本: {:?}", crate::instance::javainstall::MAJORS)));
    }
    let job_id = uuid::Uuid::new_v4().to_string();
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), crate::jobs::Job::new(job_id.clone()));
    let st2 = state.clone();
    let jid = job_id.clone();
    tokio::spawn(async move {
        crate::instance::javainstall::install(&st2, &jid, major).await;
    });
    Ok(Json(json!({ "job_id": job_id })))
}

/// 扫描本机所有 Java 安装（含启动器自带 JRE），按版本倒序
async fn javas(State(state): State<AppState>) -> Json<serde_json::Value> {
    let dir = state
        .config
        .read()
        .await
        .instances_dir()
        .parent()
        .map(|p| p.to_path_buf());
    let javas = dir
        .as_ref()
        .map(|d| crate::java_scan::load_cached(d))
        .unwrap_or_default();
    let scanned = dir.map(|d| crate::java_scan::cached_file_exists(&d)).unwrap_or(false);
    Json(json!({ "javas": javas, "scanned": scanned }))
}

/// 手动触发扫描并持久化结果
async fn javas_scan(State(state): State<AppState>) -> Json<serde_json::Value> {
    let list = crate::java_scan::scan().await;
    let dir = state
        .config
        .read()
        .await
        .instances_dir()
        .parent()
        .map(|p| p.to_path_buf());
    if let Some(d) = &dir {
        let _ = crate::java_scan::save_cached(d, &list);
    }
    Json(json!({ "javas": list }))
}

async fn java_version(Query(q): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
    let path = q
        .get("path")
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "java".to_string());
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("-version");
    crate::util::no_window(&mut cmd);
    match cmd.output().await {
        Ok(o) => {
            let text = if o.stderr.is_empty() {
                String::from_utf8_lossy(&o.stdout).to_string()
            } else {
                String::from_utf8_lossy(&o.stderr).to_string()
            };
            let version = text.lines().next().unwrap_or("").trim().to_string();
            Json(json!({ "ok": true, "version": version }))
        }
        Err(e) => Json(json!({ "ok": false, "version": format!("无法执行: {e}") })),
    }
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(asset) = Assets::get(path) {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, asset.metadata.mimetype())
            .body(Body::from(asset.data.to_vec()))
            .unwrap();
    }
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // SPA 兜底
    match Assets::get("index.html") {
        Some(a) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(a.data.to_vec()))
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "index.html missing").into_response(),
    }
}

/// 操作审计：记录写操作与失败请求（敏感参数脱敏）
async fn audit_mw(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let mutating = matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    if mutating || status >= 400 {
        crate::audit::record(&state, &method, &path, status).await;
    }
    resp
}

/// 审计日志查询
async fn audit_query(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let limit = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(200);
    let q = q.get("q").map(|s| s.as_str()).unwrap_or("");
    Json(json!({ "entries": crate::audit::query(&state, limit, q).await }))
}
