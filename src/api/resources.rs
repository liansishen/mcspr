use crate::error::{ApiError, ApiResult};
use crate::instance::files;
use crate::instance::mods;
use crate::instance::process;
use crate::instance::properties::{self, PropEntry};
use crate::instance::users;
use crate::instance::{get_instance, InstanceRuntime, Status};
use crate::instance::whitelist_sync;
use crate::state::AppState;
use axum::extract::{Multipart, Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
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
    let mut failed = Vec::new();
    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let Some(fname) = field.file_name() else { continue };
        let clean = fname.replace('\\', "/");
        let clean = clean.rsplit('/').next().unwrap_or("").to_string();
        if !clean.ends_with(".jar") {
            failed.push(json!({ "name": clean, "error": "仅支持 .jar 文件" }));
            continue;
        }
        let safe = match clean_file_name(&clean) {
            Ok(s) => s,
            Err(e) => {
                failed.push(json!({ "name": clean, "error": e.to_string() }));
                continue;
            }
        };
        match stage_upload(&dir, &safe, &mut field).await {
            Ok(()) => saved.push(safe),
            Err(e) => failed.push(json!({ "name": safe, "error": e })),
        }
    }
    if saved.is_empty() && failed.is_empty() {
        return Err(ApiError::bad_request("未收到文件"));
    }
    Ok(Json(json!({ "ok": !saved.is_empty(), "saved": saved, "failed": failed })))
}

fn clean_file_name(name: &str) -> ApiResult<String> {
    if name.is_empty() || name.contains("..") {
        return Err(ApiError::bad_request("非法文件名"));
    }
    Ok(name.to_string())
}

/// 上传落盘：先写同目录临时文件，成功后原子重命名，避免半截文件；
/// 失败时清理临时文件。
pub(crate) async fn stage_upload(
    dir: &std::path::Path,
    name: &str,
    field: &mut axum::extract::multipart::Field<'_>,
) -> Result<(), String> {
    let tmp = dir.join(format!(".upload-{}.tmp", uuid::Uuid::new_v4()));
    let result: Result<(), String> = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        tokio::fs::rename(&tmp, dir.join(name))
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
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
    let mut failed = Vec::new();
    while let Some(mut field) = multipart.next_field().await? {
        let Some(fname) = field.file_name() else { continue };
        let clean = fname.replace('\\', "/");
        let clean = clean.rsplit('/').next().unwrap_or("").to_string();
        if clean.is_empty() || clean.contains("..") {
            failed.push(json!({ "name": clean, "error": "非法文件名" }));
            continue;
        }
        match stage_upload(&dir, &clean, &mut field).await {
            Ok(()) => saved.push(clean),
            Err(e) => failed.push(json!({ "name": clean, "error": e })),
        }
    }
    if saved.is_empty() && failed.is_empty() {
        return Err(ApiError::bad_request("未收到文件"));
    }
    Ok(Json(json!({ "ok": !saved.is_empty(), "saved": saved, "failed": failed })))
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
        // 人工新增即使条目已存在，也记录人工保留标记，避免后续撤权误删
        if req.action == "whitelist_add" {
            let _ = whitelist_sync::mark_manual_retained(&rt, &target).await;
        }
        return Ok(Json(json!({ "ok": true, "mode": "command" })));
    }

    // 服务器未运行：直接读写 JSON 文件
    let dir = rt.dir.clone();
    let warning = match req.action.as_str() {
        "op" => users::add_op(&state, &dir, &target).await?,
        "whitelist_add" => {
            let warning = users::add_whitelist(&state, &dir, &target).await?;
            let _ = whitelist_sync::mark_manual_retained(&rt, &target).await;
            warning
        }
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
    #[serde(default)]
    pub sha1: String,
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
    let size = crate::instance::moddb::download_mod(&state, &dir, &req.url, &filename, &req.sha1)
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
            let (sha1, murmur) = tokio::task::spawn_blocking(move || {
                use sha1::{Digest, Sha1};
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .ok()?
                    .read_to_end(&mut bytes)
                    .ok()?;
                let mut hasher = Sha1::new();
                hasher.update(&bytes);
                let sha1 = format!("{:x}", hasher.finalize());
                let murmur = crate::instance::moddb::murmur2(&bytes).to_string();
                Some((sha1, murmur))
            })
            .await
            .unwrap_or(None)
            .unwrap_or_default();
            out.push(json!({ "filename": filename, "sha1": sha1, "murmur2": murmur }));
        }
    }
    Ok(Json(json!({ "files": out })))
}

/// 通过哈希批量查询已安装模组（modrinth: SHA1 → 版本；curseforge: murmur2 指纹 → 项目ID）
pub async fn moddb_version_files(
    State(state): State<AppState>,
    Json(req): Json<ModDownloadReq2>,
) -> ApiResult<Json<Value>> {
    let source = req.source.as_deref().unwrap_or("modrinth");
    if source == "curseforge" {
        let prints: Vec<u32> =
            req.hashes.iter().filter_map(|s| s.parse::<u32>().ok()).collect();
        if prints.is_empty() {
            return Ok(Json(json!({})));
        }
        let v = crate::instance::moddb::cf_fingerprints(&state, &prints)
            .await
            .map_err(ApiError::bad_request)?;
        // 归一化为 { "<指纹>": { "project_id": "<modId>", "filename": "<fileName>" } }
        let mut out = json!({});
        if let Some(arr) = v.pointer("/data/exactFingerprints").and_then(|x| x.as_array()) {
            for e in arr {
                let fp = e.get("id").and_then(|x| x.as_u64()).unwrap_or(0).to_string();
                let pid = e
                    .pointer("/file/modId")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0)
                    .to_string();
                let filename = e
                    .pointer("/file/fileName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if fp != "0" && pid != "0" {
                    out[fp.as_str()] = json!({ "project_id": pid, "filename": filename });
                }
            }
        }
        return Ok(Json(out));
    }
    let v = crate::instance::moddb::version_files(&state, &req.hashes)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ModDownloadReq2 {
    #[serde(default)]
    pub source: Option<String>,
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

// ---------- 可观测性 ----------

/// CPU/内存历史（最多 2880 点，降采样到约 300 点）
pub async fn metrics(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let m = rt.metrics.lock().await;
    let step = (m.len() / 300).max(1);
    let points: Vec<(i64, f32, f64)> = m.iter().step_by(step).cloned().collect();
    Ok(Json(json!({ "points": points })))
}

/// 崩溃归档文件列表
pub async fn crashes_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.join("crash-archive");
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(Json(json!({ "files": [] }))),
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        let size = e.metadata().await.map(|m| m.len()).unwrap_or(0);
        out.push(json!({ "name": name, "size": size }));
    }
    out.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
    Ok(Json(json!({ "files": out })))
}

/// 崩溃归档文件内容
pub async fn crashes_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let name = q.get("name").ok_or_else(|| ApiError::bad_request("缺少 name 参数"))?;
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(ApiError::bad_request("文件名不合法"));
    }
    let content = tokio::fs::read_to_string(rt.dir.join("crash-archive").join(name))
        .await
        .map_err(|e| ApiError::not_found(format!("读取失败: {e}")))?;
    Ok(Json(json!({ "name": name, "content": content })))
}

/// 玩家在线时长排行（实时累计：已结算时长 + 进行中的会话）
pub async fn playtime(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let stats = crate::instance::playtime_snapshot(&rt).await;
    Ok(Json(json!({ "players": stats })))
}
