mod accounts;
mod applications;
mod announcement;
mod auth;
mod backup;
mod game_backup;
mod jobs;
mod extras;
mod console;
mod instances;
mod overview;
mod permissions;
mod public_operations;
mod resources;
mod whitelist_sync;
pub(crate) use whitelist_sync::spawn_scheduler;
#[cfg(test)]
mod tests;

use crate::audit::AuditActor;
use crate::auth::Identity;
use crate::config;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Json, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

pub fn router(state: AppState) -> Router {
    // ping 无需鉴权，前端用它获取版本与账户初始化状态
    let ping = Router::new()
        .route("/api/ping", get(ping))
        .with_state(state.clone());

    let api = Router::new()
        .route(
            "/auth/login",
            post(auth::login).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/password", put(auth::password).layer(DefaultBodyLimit::max(8 * 1024)))
        .route("/auth/registration-config", get(auth::registration_config))
        .route(
            "/auth/register",
            post(auth::register).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/auth/application/status",
            post(auth::application_status).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/auth/application/resubmit",
            post(auth::application_resubmit).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route("/auth/profile", get(auth::profile))
        .route(
            "/auth/minecraft-name-requests",
            post(auth::create_name_request).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/auth/minecraft-name-requests/{id}",
            delete(auth::delete_name_request).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/accounts",
            get(accounts::list)
                .post(accounts::create)
                .layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/accounts/{id}",
            patch(accounts::patch)
                .delete(accounts::remove)
                .layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/accounts/{id}/password",
            put(accounts::reset_password).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/accounts/{id}/instances",
            put(accounts::set_instances).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route("/applications", get(applications::list))
        .route(
            "/applications/{id}/approve",
            post(applications::approve).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/applications/{id}/reject",
            post(applications::reject).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route("/announcements/preview", post(announcement::preview))
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
        .route("/instances/{id}/modpack/preview", post(instances::modpack_preview))
        .route("/instances/{id}/modpack/apply", post(instances::modpack_apply))
        .route("/instances/import/path", post(instances::import_path))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::detail))
        .route("/jobs/{id}/retry", post(jobs::retry))
        .route("/operations/{id}", get(jobs::operation_get))
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
        .route(
            "/instances/{id}/permissions",
            get(permissions::get)
                .put(permissions::put)
                .layer(DefaultBodyLimit::max(8 * 1024)),
        )
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
        .route(
            "/instances/{id}/game-backups",
            get(game_backup::list).post(game_backup::create),
        )
        .route(
            "/instances/{id}/game-backups/config",
            post(game_backup::update_config),
        )
        .route(
            "/instances/{id}/game-backups/{name}",
            get(game_backup::download).delete(game_backup::delete),
        )
        .route(
            "/instances/{id}/game-backups/{name}/preview",
            get(game_backup::preview),
        )
        .route(
            "/instances/{id}/game-backups/{name}/restore",
            post(game_backup::restore),
        )
        .route("/java", get(java_version))
        .route("/audit", get(audit_query))
        .route("/instances/{id}/console/download", get(extras::console_download))
        .route("/instances/{id}/clone", post(extras::clone_instance))
        .route("/instances/{id}/reinstall", post(extras::reinstall))
        .route("/instances/{id}/icon", get(extras::icon_get).post(extras::icon_upload))
        .route("/instances/{id}/files/download", get(extras::files_download))
        .route("/instances/{id}/files/archive", post(extras::files_archive))
        .route("/instances/{id}/files/archive-download", get(extras::files_archive_download))
        .route("/instances/{id}/files/extract", post(extras::files_extract))
        .route("/instances/{id}/worlds", get(extras::worlds_list))
        .route("/instances/{id}/worlds/switch", post(extras::worlds_switch))
        .route("/instances/{id}/worlds/create", post(extras::worlds_create))
        .route("/instances/{id}/worlds/clone", post(extras::worlds_clone))
        .route("/instances/{id}/worlds/delete", post(extras::worlds_delete))
        .route("/instances/{id}/tasks", get(extras::tasks_list).post(extras::tasks_create))
        .route("/instances/{id}/tasks/update", post(extras::tasks_update))
        .route("/instances/{id}/configs", get(extras::configs_list))
        .route("/config/export", get(config_export))
        .route("/config/import", post(config_import))
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
        .route("/instances/{id}/overview", get(overview::get))
        .route(
            "/instances/{id}/announcement",
            get(announcement::get).put(announcement::put),
        )
        .route("/instances/{id}/whitelist-sync", get(whitelist_sync::get))
        .route(
            "/instances/{id}/whitelist-sync/retry",
            post(whitelist_sync::retry),
        )
        .layer(middleware::from_fn_with_state(state.clone(), jobs::operation_mw))
        .layer(middleware::from_fn_with_state(state.clone(), public_operations::middleware))
        .layer(middleware::from_fn_with_state(state.clone(), auth_mw))
        .layer(middleware::from_fn_with_state(state.clone(), audit_mw))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state);

    Router::new()
        .merge(ping)
        .nest("/api", api)
        .fallback(static_handler)
}

const SESSION_COOKIE: &str = "mcspr_session";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    /// 无需登录（登录接口）
    Public,
    /// 任意已登录账户
    Authenticated,
    /// 仅管理员
    Admin,
}

/// 按 HTTP 方法 + 路径匹配路由，返回所需权限级别与实例 ID（如有）。
/// 除明确列入只读集合的接口外，一律要求管理员。
fn classify(method: &Method, path: &str) -> (Access, Option<String>) {
    let p = path.strip_prefix("/api").unwrap_or(path);
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let get = method == Method::GET;
    let post = method == Method::POST;
    let put = method == Method::PUT;
    match segs.as_slice() {
        ["auth", "login"] if post => (Access::Public, None),
        ["auth", "me"] if get => (Access::Authenticated, None),
        ["auth", "logout"] if post => (Access::Authenticated, None),
        ["auth", "password"] if put => (Access::Authenticated, None),
        ["instances"] if get => (Access::Authenticated, None),
        ["instances", id] if get => (Access::Authenticated, Some((*id).to_string())),
        ["instances", id, sub]
            if get && matches!(*sub, "status" | "overview" | "playtime" | "announcement") =>
        {
            (Access::Authenticated, Some((*id).to_string()))
        }
        ["auth", "registration-config"] if get => (Access::Public, None),
        ["auth", "register"] if post => (Access::Public, None),
        ["auth", "application", "status"] if post => (Access::Public, None),
        ["auth", "application", "resubmit"] if post => (Access::Public, None),
        ["auth", "profile"] if get => (Access::Authenticated, None),
        ["auth", "minecraft-name-requests"] if post => (Access::Authenticated, None),
        ["auth", "minecraft-name-requests", _] if method == Method::DELETE => {
            (Access::Authenticated, None)
        }
        ["jobs"] if get => (Access::Authenticated, None),
        ["jobs", _id] if get => (Access::Authenticated, None),
        ["jobs", _id, "retry"] if post => (Access::Authenticated, None),
        ["operations", _id] if get => (Access::Authenticated, None),
        // 白名单同步状态 / 重试仅管理员可见（也便于后续调整默认级别）
        ["instances", _id, "whitelist-sync"] if get => (Access::Admin, None),
        ["instances", _id, "whitelist-sync", "retry"] if post => (Access::Admin, None),
        _ => (Access::Admin, None),
    }
}

/// 认证与授权中间件：解析会话 Cookie、校验来源与 CSRF、按路由要求授权实例。
/// 身份以最新账户状态构建，撤销授权 / 禁用 / 改角色在下次请求即生效。
async fn auth_mw(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let (access, instance_id) = classify(req.method(), req.uri().path());
    if access == Access::Public {
        return next.run(req).await;
    }

    let identity = match cookie_value(req.headers(), SESSION_COOKIE) {
        Some(token) => state.auth.session_identity(&token).await,
        None => None,
    };
    let Some(identity) = identity else {
        return api_error(StatusCode::UNAUTHORIZED, "未登录或会话已失效");
    };

    // 同源校验：浏览器带 Origin 时须与 Host 一致；无 Origin（非浏览器客户端）放行
    if !origin_ok(req.headers()) {
        return deny(&identity, StatusCode::FORBIDDEN, "请求来源不受信任");
    }

    // 写请求 CSRF（登录为 Public，不需要）
    if is_mutating(req.method()) {
        let ok = req
            .headers()
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .map(|v| crate::util::ct_eq(v, &identity.csrf))
            .unwrap_or(false);
        if !ok {
            return deny(&identity, StatusCode::FORBIDDEN, "CSRF 校验失败");
        }
    }

    if access == Access::Admin && !identity.is_admin() {
        return deny(&identity, StatusCode::FORBIDDEN, "需要管理员权限");
    }

    if let Some(id) = instance_id {
        if !identity.can_view(&id) {
            // 未授权实例与不存在实例统一返回 404，减少实例枚举
            return deny(&identity, StatusCode::NOT_FOUND, "实例不存在");
        }
    }

    req.extensions_mut().insert(identity.clone());
    let mut resp = next.run(req).await;
    // 供审计中间件读取当前操作者（成功与被拒请求都记录）
    resp.extensions_mut().insert(AuditActor::from_identity(&identity));
    resp
}

fn is_mutating(method: &Method) -> bool {
    matches!(method, &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        if let Some((k, v)) = pair.trim().split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn session_cookie(token: &str, secure: bool, max_age: i64) -> String {
    let mut c = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}"
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// 是否 HTTPS（通过反向代理头判断）；HTTP / SSH 隧道场景不加 Secure。
fn is_secure_request(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

/// 同源校验：浏览器带 Origin 时必须与 Host 一致，`null` 一律拒绝；
/// 无 Origin 头（非浏览器客户端 / 自动化测试）放行。
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if origin == "null" {
        return false;
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or("");
    origin_host.eq_ignore_ascii_case(host)
}

pub(crate) fn api_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// 返回错误响应并附带已知操作者，保证被拒请求的审计仍记录身份。
fn deny(identity: &Identity, status: StatusCode, msg: impl Into<String>) -> Response {
    let mut resp = api_error(status, msg);
    resp.extensions_mut().insert(AuditActor::from_identity(identity));
    resp
}

/// 返回错误响应并附带显式操作者（登录接口用尝试的用户名）。
pub(crate) fn api_error_actor(
    status: StatusCode,
    msg: impl Into<String>,
    actor: AuditActor,
) -> Response {
    let mut resp = api_error(status, msg);
    resp.extensions_mut().insert(actor);
    resp
}

async fn ping(State(state): State<AppState>) -> Json<serde_json::Value> {
    // 认证始终必需；`initialized` 表示是否已创建账户（未初始化时业务接口保持封闭）
    let initialized = state.auth.has_users().await;
    let registration_enabled = state.config.read().await.registration_enabled;
    Json(json!({
        "ok": true,
        "name": "MCS Panel",
        "version": env!("CARGO_PKG_VERSION"),
        "auth_required": true,
        "initialized": initialized,
        "registration_enabled": registration_enabled,
    }))
}

#[derive(Deserialize)]
struct SettingsUpdate {
    listen: Option<String>,
    token: Option<String>,
    data_dir: Option<String>,
    curseforge_api_key: Option<String>,
    telegram_bot_token: Option<String>,
    console_max_lines: Option<usize>,
    console_buffer_lines: Option<usize>,
    registration_enabled: Option<bool>,
    turnstile_site_key: Option<String>,
    turnstile_secret_key: Option<String>,
    turnstile_allowed_hostnames: Option<Vec<String>>,
    turnstile_test_mode: Option<bool>,
}

/// 设置读取：机密字段（token / CurseForge Key / Telegram Token）脱敏返回，
/// 只回 `*_set` 标志，避免机密常驻前端 DOM
async fn get_settings(State(state): State<AppState>) -> Json<serde_json::Value> {
    let c = state.config.read().await;
    Json(json!({
        "listen": c.listen,
        "data_dir": c.data_dir,
        "backup_keep": c.backup_keep,
        "backup_keep_days": c.backup_keep_days,
        "alert_type": c.alert_type,
        "alert_webhook_url": c.alert_webhook_url,
        "telegram_chat_id": c.telegram_chat_id,
        "token": "",
        "token_set": !c.token.is_empty(),
        "curseforge_api_key": "",
        "curseforge_api_key_set": !c.curseforge_api_key.is_empty(),
        "telegram_bot_token": "",
        "telegram_bot_token_set": !c.telegram_bot_token.is_empty(),
        "console_max_lines": c.console_max_lines,
        "console_buffer_lines": c.console_buffer_lines,
        "console_lines_range": [*config::CONSOLE_LINES_RANGE.start(), *config::CONSOLE_LINES_RANGE.end()],
        "console_buffer_range": [*config::CONSOLE_BUFFER_RANGE.start(), *config::CONSOLE_BUFFER_RANGE.end()],
        "registration_enabled": c.registration_enabled,
        "turnstile_site_key": c.turnstile_site_key,
        "turnstile_secret_key": "",
        "turnstile_secret_key_set": !c.turnstile_secret_key.is_empty(),
        "turnstile_allowed_hostnames": c.turnstile_allowed_hostnames,
        "turnstile_test_mode": c.turnstile_test_mode,
    }))
}

async fn put_settings(
    State(state): State<AppState>,
    Json(u): Json<SettingsUpdate>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut cfg = state.config.read().await.clone();
    if let Some(l) = u.listen {
        if !l.trim().is_empty() {
            cfg.listen = l.trim().to_string();
        }
    }
    // 机密字段：留空 = 保持不变（配合前端脱敏显示）；如需清除请编辑 config.toml
    if let Some(t) = u.token {
        let t = t.trim().to_string();
        if !t.is_empty() {
            cfg.token = t;
        }
    }
    if let Some(d) = u.data_dir {
        if !d.trim().is_empty() {
            cfg.data_dir = d.trim().to_string();
        }
    }
    if let Some(k) = u.curseforge_api_key {
        let k = k.trim().to_string();
        if !k.is_empty() {
            cfg.curseforge_api_key = k;
        }
    }
    if let Some(n) = u.console_max_lines {
        if !config::CONSOLE_LINES_RANGE.contains(&n) {
            return Err(ApiError::bad_request(format!(
                "控制台显示行数需在 {}~{} 之间",
                config::CONSOLE_LINES_RANGE.start(),
                config::CONSOLE_LINES_RANGE.end()
            )));
        }
        cfg.console_max_lines = n;
    }
    if let Some(n) = u.console_buffer_lines {
        if !config::CONSOLE_BUFFER_RANGE.contains(&n) {
            return Err(ApiError::bad_request(format!(
                "控制台缓存行数需在 {}~{} 之间",
                config::CONSOLE_BUFFER_RANGE.start(),
                config::CONSOLE_BUFFER_RANGE.end()
            )));
        }
        cfg.console_buffer_lines = n;
    }
    if let Some(k) = u.telegram_bot_token {
        let k = k.trim().to_string();
        if !k.is_empty() {
            cfg.telegram_bot_token = k;
        }
    }
    if let Some(v) = u.registration_enabled {
        cfg.registration_enabled = v;
    }
    if let Some(k) = u.turnstile_site_key {
        cfg.turnstile_site_key = k.trim().to_string();
    }
    if let Some(k) = u.turnstile_secret_key {
        let k = k.trim().to_string();
        if !k.is_empty() {
            cfg.turnstile_secret_key = k;
        }
    }
    if let Some(list) = u.turnstile_allowed_hostnames {
        let mut cleaned: Vec<String> = Vec::new();
        for h in list {
            let h = h.trim().to_string();
            if !h.is_empty() && !cleaned.iter().any(|x| x.eq_ignore_ascii_case(&h)) {
                cleaned.push(h);
            }
        }
        cfg.turnstile_allowed_hostnames = cleaned;
    }
    if let Some(v) = u.turnstile_test_mode {
        cfg.turnstile_test_mode = v;
    }
    // 开启注册前必须完成有效的人机验证配置（失败关闭）
    validate_registration_config(&cfg)?;
    config::save(&cfg)?;
    *state.config.write().await = cfg;
    // 控制台缓存上限对运行中的实例立即生效
    {
        let limit = state.config.read().await.console_buffer_lines;
        for rt in state.instances.read().await.values() {
            rt.log_limit
                .store(limit.max(1), std::sync::atomic::Ordering::Relaxed);
        }
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
async fn java_install(
    State(state): State<AppState>,
    Path(major): Path<u32>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<serde_json::Value>> {
    if !crate::instance::javainstall::MAJORS.contains(&major) {
        return Err(ApiError::bad_request(format!("仅支持安装以下大版本: {:?}", crate::instance::javainstall::MAJORS)));
    }
    let job_id = crate::jobs::create_job(
        &state,
        crate::jobs::NewJob {
            kind: "java-install".into(),
            title: format!("安装 Java {major}"),
            instance_id: None,
            user_id: Some(identity.user_id.clone()),
            operation_id: None,
        },
    );
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
            // 内嵌资源随版本更新变化：禁止缓存，避免面板升级后浏览器仍用旧前端
            .header(header::CACHE_CONTROL, "no-cache")
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
        // auth_mw / 登录接口在响应扩展中留下操作者信息
        let actor = resp.extensions().get::<AuditActor>().cloned();
        let target = audit_target(&path);
        crate::audit::record(&state, &method, &path, status, actor.as_ref(), target.as_deref()).await;
    }
    resp
}

/// 从路径提取审计目标（实例 / 账户 ID）。
fn audit_target(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let i = if segs.first() == Some(&"api") { 1 } else { 0 };
    let kind = *segs.get(i)?;
    if kind == "instances" || kind == "accounts" {
        let id = *segs.get(i + 1)?;
        Some(format!("{kind}:{id}"))
    } else {
        None
    }
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

async fn config_export(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut cfg = state.config.read().await.clone();
    redact_secrets(&mut cfg);
    let map = state.instances.read().await;
    let mut metas = Vec::new();
    for rt in map.values() {
        metas.push(rt.meta.read().await.clone());
    }
    drop(map);
    Json(json!({ "version": env!("CARGO_PKG_VERSION"), "settings": cfg, "instances": metas }))
}

/// 导出配置前抹除机密字段，避免秘密随导出泄露。
fn redact_secrets(cfg: &mut crate::config::PanelConfig) {
    cfg.token = String::new();
    cfg.curseforge_api_key = String::new();
    cfg.telegram_bot_token = String::new();
    cfg.turnstile_secret_key = String::new();
}

/// 注册配置校验：开启注册时人机验证必须配置有效（失败关闭）。
/// `put_settings` 与 `config_import` 共用，避免导入配置绕过注册校验。
fn validate_registration_config(cfg: &crate::config::PanelConfig) -> Result<(), ApiError> {
    if cfg.registration_enabled {
        crate::captcha::TurnstileSettings::from_config(cfg)
            .is_configured()
            .map_err(ApiError::bad_request)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct ConfigImport {
    settings: Option<crate::config::PanelConfig>,
    #[serde(default)]
    instances: Vec<crate::instance::InstanceMeta>,
}

async fn config_import(
    State(state): State<AppState>,
    Json(data): Json<ConfigImport>,
) -> ApiResult<Json<serde_json::Value>> {
    let ConfigImport { settings, instances } = data;
    let mut settings = settings;
    // 先合并机密字段并做与 put_settings 相同的注册配置校验，
    // 避免非法设置写入磁盘后才失败，或绕过注册校验。
    if let Some(cfg) = settings.as_mut() {
        if cfg.listen.trim().is_empty() {
            return Err(ApiError::bad_request("导入的 settings 缺少 listen"));
        }
        // 脱敏导出的机密字段为空时保留现值，避免导入清除密钥
        {
            let current = state.config.read().await;
            if cfg.token.is_empty() {
                cfg.token = current.token.clone();
            }
            if cfg.curseforge_api_key.is_empty() {
                cfg.curseforge_api_key = current.curseforge_api_key.clone();
            }
            if cfg.telegram_bot_token.is_empty() {
                cfg.telegram_bot_token = current.telegram_bot_token.clone();
            }
            if cfg.turnstile_secret_key.is_empty() {
                cfg.turnstile_secret_key = current.turnstile_secret_key.clone();
            }
        }
        validate_registration_config(cfg)?;
    }
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut imported = 0u32;
    for meta in &instances {
        if meta.id.is_empty() {
            continue;
        }
        let dir = state.config.read().await.instances_dir().join(&meta.id);
        std::fs::create_dir_all(&dir).ok();
        let json_path = dir.join("instance.json");
        if json_path.exists() {
            let _ = std::fs::rename(&json_path, dir.join(format!("instance.json.bak-{ts}")));
        }
        std::fs::write(&json_path, serde_json::to_string_pretty(meta)?).ok();
        imported += 1;
    }
    let mut note = String::new();
    if let Some(cfg) = settings {
        let cfg_path = std::path::Path::new("config.toml");
        if cfg_path.exists() {
            let _ = std::fs::copy(cfg_path, std::path::Path::new(&format!("config.toml.bak-{ts}")));
        }
        crate::config::save(&cfg)?;
        *state.config.write().await = cfg;
        note.push_str("设置已写入（listen/data_dir 需重启面板生效）；");
    }
    {
        let (dir, buffer_lines) = {
            let c = state.config.read().await;
            (c.instances_dir(), c.console_buffer_lines)
        };
        let instances = crate::instance::scan_instances(&dir, buffer_lines);
        *state.instances.write().await = instances;
    }
    Ok(Json(json!({ "ok": true, "instances": imported, "note": note })))
}
