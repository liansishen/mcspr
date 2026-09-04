use crate::error::{ApiError, ApiResult};
use crate::instance::files;
use crate::instance::mods;
use crate::instance::process;
use crate::instance::properties::{self, PropEntry};
use crate::instance::users;
use crate::instance::{get_instance, InstanceRuntime, Status};
use crate::state::AppState;
use axum::extract::{Multipart, Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

// ---------- 模组管理 ----------

pub async fn mods_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let list = mods::list(&rt).await?;
    Ok(Json(json!({ "mods": list })))
}

#[derive(Deserialize)]
pub struct FileReq {
    pub file: String,
}

pub async fn mods_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<FileReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let renamed_to = mods::toggle(&rt, &req.file).await?;
    Ok(Json(json!({ "ok": true, "renamed_to": renamed_to })))
}

pub async fn mods_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<FileReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    mods::delete(&rt, &req.file).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn mods_upload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = mods::mods_dir(&rt).await;
    tokio::fs::create_dir_all(&dir).await?;
    let mut saved = Vec::new();
    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let Some(fname) = field.file_name() else { continue };
        let clean = fname.replace('\\', "/");
        let clean = clean.rsplit('/').next().unwrap_or("").to_string();
        if !clean.ends_with(".jar") {
            return Err(ApiError::bad_request("仅支持 .jar 文件"));
        }
        let safe = clean_file_name(&clean)?;
        let path = dir.join(&safe);
        let mut file = tokio::fs::File::create(&path).await?;
        while let Some(chunk) = field.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        saved.push(safe);
    }
    if saved.is_empty() {
        return Err(ApiError::bad_request("未收到文件"));
    }
    Ok(Json(json!({ "ok": true, "saved": saved })))
}

fn clean_file_name(name: &str) -> ApiResult<String> {
    if name.is_empty() || name.contains("..") {
        return Err(ApiError::bad_request("非法文件名"));
    }
    Ok(name.to_string())
}

// ---------- 文件管理 ----------

pub async fn files_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let path = q.get("path").cloned().unwrap_or_default();
    let entries = files::list_dir(&rt.dir, &path).await?;
    Ok(Json(json!({ "path": path, "entries": entries })))
}

pub async fn file_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let path = q.get("path").ok_or_else(|| ApiError::bad_request("缺少 path 参数"))?;
    let f = files::read_file(&rt.dir, path).await?;
    Ok(Json(json!({
        "path": path,
        "binary": f.binary,
        "content": f.content,
        "size": f.size,
        "editable": f.editable,
    })))
}

#[derive(Deserialize)]
pub struct FileContentReq {
    pub path: String,
    pub content: String,
}

pub async fn file_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<FileContentReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    files::write_file(&rt.dir, &req.path, &req.content).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct PathReq {
    pub path: String,
}

pub async fn files_mkdir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PathReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    files::mkdir(&rt.dir, &req.path).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn files_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PathReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let p = files::safe_join(&rt.dir, &req.path)?;
    if p == rt.dir {
        return Err(ApiError::bad_request("不能删除实例根目录"));
    }
    files::delete(&rt.dir, &req.path).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct RenameReq {
    pub from: String,
    pub to: String,
}

pub async fn files_rename(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RenameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    files::rename(&rt.dir, &req.from, &req.to).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn files_upload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let rt: Arc<InstanceRuntime> = get_instance(&state, &id).await?;
    let dir_rel = q.get("path").cloned().unwrap_or_default();
    let dir = files::safe_join(&rt.dir, &dir_rel)?;
    if !dir.is_dir() {
        return Err(ApiError::bad_request("目标目录不存在"));
    }
    let mut saved = Vec::new();
    while let Some(mut field) = multipart.next_field().await? {
        let Some(fname) = field.file_name() else { continue };
        let clean = fname.replace('\\', "/");
        let clean = clean.rsplit('/').next().unwrap_or("").to_string();
        if clean.is_empty() || clean.contains("..") {
            continue;
        }
        let target = dir.join(&clean);
        let mut file = tokio::fs::File::create(&target).await?;
        while let Some(chunk) = field.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        saved.push(clean);
    }
    if saved.is_empty() {
        return Err(ApiError::bad_request("未收到文件"));
    }
    Ok(Json(json!({ "ok": true, "saved": saved })))
}

pub async fn jars(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let list = files::list_jars(&rt.dir);
    Ok(Json(json!({ "jars": list })))
}

// ---------- 用户管理（OP / 白名单 / 封禁） ----------

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_ip(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && s.split('.').count() == 4
        && s.split('.').all(|p| !p.is_empty() && p.parse::<u8>().is_ok())
}

pub async fn users_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let running = matches!(*rt.status.lock().await, Status::Running | Status::Starting);
    let dir = rt.dir.clone();
    let online = rt.players.lock().await.clone();
    Ok(Json(json!({
        "running": running,
        "online": online,
        "ops": users::read_array(&dir, "ops.json"),
        "whitelist": users::read_array(&dir, "whitelist.json"),
        "banned": users::read_array(&dir, "banned-players.json"),
        "bannedIps": users::read_array(&dir, "banned-ips.json"),
    })))
}

#[derive(Deserialize)]
pub struct UserActionReq {
    pub action: String,
    pub target: String,
    pub reason: Option<String>,
}

pub async fn users_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UserActionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let target = req.target.trim().to_string();
    let reason = req.reason.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let name_action = matches!(
        req.action.as_str(),
        "op" | "deop" | "whitelist_add" | "whitelist_remove" | "ban" | "pardon" | "kick"
    );
    let ip_action = matches!(req.action.as_str(), "ban_ip" | "pardon_ip");
    if !name_action && !ip_action {
        return Err(ApiError::bad_request(format!("未知操作: {}", req.action)));
    }
    if name_action && !valid_name(&target) {
        return Err(ApiError::bad_request("玩家名不合法（仅限字母、数字、下划线，1-16 字符）"));
    }
    if ip_action && !valid_ip(&target) {
        return Err(ApiError::bad_request("IP 地址不合法（例如 192.168.1.10）"));
    }

    // 服务器运行中：走控制台命令，实时生效
    if matches!(*rt.status.lock().await, Status::Running | Status::Starting) {
        let cmd = match req.action.as_str() {
            "op" => format!("op {target}"),
            "deop" => format!("deop {target}"),
            "whitelist_add" => format!("whitelist add {target}"),
            "whitelist_remove" => format!("whitelist remove {target}"),
            "ban" => match &reason {
                Some(r) => format!("ban {target} {r}"),
                None => format!("ban {target}"),
            },
            "pardon" => format!("pardon {target}"),
            "kick" => match &reason {
                Some(r) => format!("kick {target} {r}"),
                None => format!("kick {target}"),
            },
            "ban_ip" => match &reason {
                Some(r) => format!("ban-ip {target} {r}"),
                None => format!("ban-ip {target}"),
            },
            "pardon_ip" => format!("pardon-ip {target}"),
            _ => unreachable!(),
        };
        process::send_command(&rt, &cmd).await?;
        return Ok(Json(json!({ "ok": true, "mode": "command" })));
    }

    // 服务器未运行：直接读写 JSON 文件
    let dir = rt.dir.clone();
    let warning = match req.action.as_str() {
        "op" => users::add_op(&state, &dir, &target).await?,
        "whitelist_add" => users::add_whitelist(&state, &dir, &target).await?,
        "ban" => users::add_ban(&state, &dir, &target, reason).await?,
        "ban_ip" => users::add_ban_ip(&dir, &target, reason).await?,
        "deop" => {
            if !users::remove_entry(&dir, "ops.json", "name", &target).await? {
                return Err(ApiError::bad_request("该玩家不在 OP 列表中"));
            }
            None
        }
        "whitelist_remove" => {
            if !users::remove_entry(&dir, "whitelist.json", "name", &target).await? {
                return Err(ApiError::bad_request("该玩家不在白名单中"));
            }
            None
        }
        "pardon" => {
            if !users::remove_entry(&dir, "banned-players.json", "name", &target).await? {
                return Err(ApiError::bad_request("该玩家未被封禁"));
            }
            None
        }
        "kick" => {
            return Err(ApiError::bad_request("踢出玩家需要服务器处于运行状态"));
        }
        "pardon_ip" => {
            if !users::remove_entry(&dir, "banned-ips.json", "ip", &target).await? {
                return Err(ApiError::bad_request("该 IP 未被封禁"));
            }
            None
        }
        _ => unreachable!(),
    };
    Ok(Json(json!({ "ok": true, "mode": "file", "warning": warning })))
}

// ---------- 模组下载（Modrinth / CurseForge） ----------

pub async fn moddb_search(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let source = q.get("source").map(|s| s.as_str()).unwrap_or("modrinth");
    let query = q.get("q").map(|s| s.as_str()).unwrap_or("");
    let game = q.get("game").map(|s| s.as_str()).unwrap_or("");
    let loader = q.get("loader").map(|s| s.as_str()).unwrap_or("");
    let results = crate::instance::moddb::search(&state, source, query, game, loader)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "results": results })))
}

pub async fn moddb_versions(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let source = q.get("source").map(|s| s.as_str()).unwrap_or("modrinth");
    let project = q.get("project").ok_or_else(|| ApiError::bad_request("缺少 project 参数"))?;
    let game = q.get("game").map(|s| s.as_str()).unwrap_or("");
    let loader = q.get("loader").map(|s| s.as_str()).unwrap_or("");
    let versions = crate::instance::moddb::versions(&state, source, project, game, loader)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "versions": versions })))
}

#[derive(Deserialize)]
pub struct ModDownloadReq {
    pub url: String,
    pub filename: String,
}

pub async fn mods_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ModDownloadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let filename = req.filename.trim().to_string();
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || !filename.to_lowercase().ends_with(".jar")
    {
        return Err(ApiError::bad_request("文件名不合法（需为 .jar）"));
    }
    let dir = mods::mods_dir(&rt).await;
    tokio::fs::create_dir_all(&dir).await?;
    let size = crate::instance::moddb::download_mod(&state, &dir, &req.url, &filename)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "ok": true, "filename": filename, "size": size })))
}

/// 批量查询模组的客户端/服务端支持情况（仅 Modrinth 提供）
pub async fn moddb_projects(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let source = q.get("source").map(|s| s.as_str()).unwrap_or("modrinth");
    let ids: Vec<String> = q
        .get("ids")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    if source != "modrinth" {
        return Ok(Json(json!({ "projects": [] })));
    }
    let projects = crate::instance::moddb::projects_sides(&state, &ids)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "projects": projects })))
}

/// mods 目录中所有模组文件的 SHA1（用于在线市场识别已安装模组）
pub async fn mods_hashes(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = mods::mods_dir(&rt).await;
    let mut out = Vec::new();
    if dir.is_dir() {
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(e) = rd.next_entry().await? {
            let filename = e.file_name().to_string_lossy().to_string();
            if !filename.ends_with(".jar") && !filename.ends_with(".jar.disabled") {
                continue;
            }
            let path = e.path();
            let sha1 = tokio::task::spawn_blocking(move || {
                use sha1::{Digest, Sha1};
                let mut f = std::fs::File::open(&path).ok()?;
                let mut hasher = Sha1::new();
                std::io::copy(&mut f, &mut hasher).ok()?;
                Some(format!("{:x}", hasher.finalize()))
            })
            .await
            .unwrap_or(None)
            .unwrap_or_default();
            out.push(json!({ "filename": filename, "sha1": sha1 }));
        }
    }
    Ok(Json(json!({ "files": out })))
}

/// 通过 SHA1 批量查询 Modrinth 版本（识别已安装）
pub async fn moddb_version_files(
    State(state): State<AppState>,
    Json(req): Json<ModDownloadReq2>,
) -> ApiResult<Json<Value>> {
    let v = crate::instance::moddb::version_files(&state, &req.hashes)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ModDownloadReq2 {
    pub hashes: Vec<String>,
}

// ---------- server.properties ----------

pub async fn props_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let p = properties::read(&rt.dir);
    Ok(Json(json!({ "exists": p.exists, "entries": p.entries })))
}

#[derive(Deserialize)]
pub struct PropsPutReq {
    pub entries: Vec<PropEntry>,
}

pub async fn props_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PropsPutReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    properties::write(&rt.dir, &req.entries).await?;
    Ok(Json(json!({ "ok": true })))
}
