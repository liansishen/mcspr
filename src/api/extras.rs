//! 体验增强端点：控制台下载 / 克隆 / 重装 / 图标 / 文件下载与打包解压 / 世界管理 / 计划任务 / 配置直达

use crate::error::{ApiError, ApiResult};
use crate::instance::{files, get_instance, loaders, properties, vanilla, InstanceMeta, InstanceRuntime};
use crate::state::AppState;
use axum::extract::{Multipart, Path, Query, State};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use std::path::Path as StdPath;

// ---------- 控制台日志下载 ----------

pub async fn console_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let rt = get_instance(&state, &id).await?;
    let buf = rt.log_buf.lock().await;
    let mut text = String::new();
    for l in buf.iter() {
        text.push_str(&l.ts);
        text.push(' ');
        text.push_str(&l.line);
        text.push('\n');
    }
    drop(buf);
    let mut resp = Response::new(axum::body::Body::from(text));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"console-{id}.log\""))
            .unwrap(),
    );
    Ok(resp)
}

// ---------- 一键克隆 ----------

fn copy_dir_all(src: &StdPath, dst: &StdPath) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir_all(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

fn next_free_port(used: &[u32]) -> u32 {
    for p in 25565..=25599 {
        if !used.contains(&p) && std::net::TcpListener::bind(("127.0.0.1", p as u16)).is_ok() {
            return p;
        }
    }
    25565
}

#[derive(Deserialize)]
pub struct CloneReq {
    pub name: String,
}

pub async fn clone_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CloneReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err(ApiError::bad_request("克隆前请先停止实例"));
    }
    let new_id = uuid::Uuid::new_v4().to_string();
    let new_dir = state.config.read().await.instances_dir().join(&new_id);
    let new_dir_clone = new_dir.clone();
    let src = rt.dir.clone();
    tokio::task::spawn_blocking(move || copy_dir_all(&src, &new_dir_clone))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::internal(format!("复制目录失败: {e}")))?;

    // 分配空闲端口（避开其它实例占用的端口）
    let mut used: Vec<u32> = Vec::new();
    for other in state.instances.read().await.values() {
        let props = other.dir.join("server.properties");
        if let Ok(text) = std::fs::read_to_string(props) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == "server-port" {
                        if let Ok(p) = v.trim().parse::<u32>() {
                            used.push(p);
                        }
                    }
                }
            }
        }
    }
    let port = next_free_port(&used);
    properties::set_values(
        &new_dir,
        &[(("server-port").to_string(), port.to_string())],
    )
    .await
    .ok();

    let m = rt.meta.read().await.clone();
    let meta = InstanceMeta {
        id: new_id.clone(),
        name: req.name.trim().to_string(),
        created_at: crate::util::now_str(),
        java_path: m.java_path,
        min_ram_mb: m.min_ram_mb,
        max_ram_mb: m.max_ram_mb,
        jar: m.jar,
        jvm_args: m.jvm_args,
        auto_restart: false,
        auto_start_on_boot: false,
        mc_version: m.mc_version,
        mod_loader: m.mod_loader,
    };
    tokio::fs::write(
        new_dir.join("instance.json"),
        serde_json::to_string_pretty(&meta)?,
    )
    .await?;
    let new_rt = InstanceRuntime::new(meta, new_dir);
    state.instances.write().await.insert(new_id.clone(), new_rt);
    Ok(Json(json!({ "id": new_id, "port": port })))
}

// ---------- 重装服务端（换类型 / 升降级，保留世界，自动备份） ----------

#[derive(Deserialize)]
pub struct ReinstallReq {
    pub server_type: String,
    pub mc_version: String,
    pub loader_version: Option<String>,
    #[serde(default = "bool_true3")]
    pub backup_first: bool,
}
fn bool_true3() -> bool {
    true
}

pub async fn reinstall(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ReinstallReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err(ApiError::bad_request("重装前请先停止实例"));
    }
    let game = req.mc_version.trim().to_string();
    if game.is_empty() {
        return Err(ApiError::bad_request("请选择 MC 版本"));
    }
    let lver = req.loader_version.clone().unwrap_or_default();
    let loader = req.server_type.trim().to_string();
    let known = matches!(
        loader.as_str(),
        "vanilla" | "fabric" | "quilt" | "forge" | "neoforge" | "paper" | "purpur" | "folia" | "velocity" | "waterfall" | "bungeecord"
    );
    if !known {
        return Err(ApiError::bad_request(format!("未知服务端类型: {loader}")));
    }
    if loader != "vanilla" && loader != "velocity" && loader != "bungeecord" && lver.trim().is_empty() {
        return Err(ApiError::bad_request("请选择服务端版本"));
    }
    {
        let mut meta = rt.meta.write().await;
        meta.mc_version = Some(game.clone());
        meta.mod_loader = if loader == "vanilla" { None } else { Some(loader.clone()) };
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), crate::jobs::Job::new(job_id.clone()));
    let st2 = state.clone();
    let jid = job_id.clone();
    let iid = id.clone();
    let backup_first = req.backup_first;
    let loader_c = loader.clone();
    let game_c = game.clone();
    let lver_c = lver.clone();
    tokio::spawn(async move {
        let Some(rt) = st2.instances.read().await.get(&iid).cloned() else {
            crate::jobs::finish_job(&st2, &jid, Some("实例不存在".into()), None);
            return;
        };
        let iname = rt.meta.read().await.name.clone();
        if backup_first {
            crate::jobs::log_job(&st2, &jid, "重装前自动备份…");
            match crate::instance::backup::create(&st2, &rt).await {
                Ok(name) => crate::jobs::log_job(&st2, &jid, format!("自动备份完成: {name}")),
                Err(e) => {
                    crate::jobs::finish_job(
                        &st2,
                        &jid,
                        Some(format!("自动备份失败: {e}（已中止重装）")),
                        Some(iid.clone()),
                    );
                    return;
                }
            }
        }
        if loader_c == "vanilla" {
            crate::instance::vanilla::download_server(&st2, &jid, &iid, &game_c).await;
        } else {
            crate::instance::loaders::install(&st2, &jid, &iid, &loader_c, &game_c, &lver_c).await;
        }
        crate::jobs::log_job(
            &st2,
            &jid,
            "✅ 重装流程结束（世界与配置保留）。若服务端类型变化，请检查 mods/plugins 目录。",
        );
    });
    Ok(Json(json!({ "job_id": job_id })))
}

// ---------- 服务器图标 ----------

pub async fn icon_upload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let mut saved = false;
    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let Some(fname) = field.file_name() else { continue };
        if !fname.to_lowercase().ends_with(".png") {
            return Err(ApiError::bad_request("仅支持 PNG 图片"));
        }
        let path = rt.dir.join("server-icon.png");
        let mut f = tokio::fs::File::create(&path).await?;
        while let Some(chunk) = field.chunk().await? {
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        saved = true;
    }
    if !saved {
        return Err(ApiError::bad_request("未收到文件"));
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn icon_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let rt = get_instance(&state, &id).await?;
    let p = rt.dir.join("server-icon.png");
    if !p.exists() {
        return Err(ApiError::not_found("未设置图标"));
    }
    let bytes = tokio::fs::read(&p).await?;
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("image/png"),
    );
    Ok(resp)
}

// ---------- 文件下载 / 打包 / 解压 ----------

pub async fn files_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Response> {
    let rt = get_instance(&state, &id).await?;
    let path = q.get("path").ok_or_else(|| ApiError::bad_request("缺少 path 参数"))?;
    let p = files::safe_join(&rt.dir, path)?;
    if !p.is_file() {
        return Err(ApiError::bad_request("不是文件"));
    }
    let fname = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let file = tokio::fs::File::open(&p).await?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Response::new(axum::body::Body::from_stream(stream));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            fname.replace('"', "")
        ))
        .unwrap(),
    );
    Ok(resp)
}

#[derive(Deserialize)]
pub struct ArchiveReq {
    pub paths: Vec<String>,
    pub name: String,
}

fn append_tree(
    dir: &StdPath,
    rel: &str,
    builder: &mut tar::Builder<&mut flate2::write::GzEncoder<std::fs::File>>,
) -> std::io::Result<()> {
    builder.append_dir(rel, dir)?;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let child_rel = format!("{rel}/{}", e.file_name().to_string_lossy());
        if e.file_type()?.is_dir() {
            append_tree(&e.path(), &child_rel, builder)?;
        } else {
            builder.append_path_with_name(e.path(), &child_rel)?;
        }
    }
    Ok(())
}

/// 多选文件/目录打包 tar.gz（存入临时目录，随后下载）
pub async fn files_archive(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ArchiveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if req.paths.is_empty() {
        return Err(ApiError::bad_request("未选择要打包的文件"));
    }
    let name = if req.name.ends_with(".tar.gz") {
        req.name.clone()
    } else {
        format!("{}.tar.gz", req.name.trim())
    };
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(ApiError::bad_request("归档名不合法"));
    }
    let tmp = StdPath::new(&state.config.read().await.data_dir).join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| ApiError::internal(e.to_string()))?;
    let out = tmp.join(&name);
    let src = rt.dir.clone();
    let paths = req.paths.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut enc);
            for p in &paths {
                let full = files::safe_join(&src, p).map_err(|e| e.to_string())?;
                let rel = p.replace('\\', "/");
                let md = std::fs::metadata(&full).map_err(|e| e.to_string())?;
                if md.is_dir() {
                    append_tree(&full, &rel, &mut builder).map_err(|e| e.to_string())?;
                } else {
                    builder
                        .append_path_with_name(&full, &rel)
                        .map_err(|e| e.to_string())?;
                }
            }
            builder.finish().map_err(|e| e.to_string())?;
        }
        enc.finish().map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "ok": true, "name": name })))
}

pub async fn files_archive_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Response> {
    let _ = get_instance(&state, &id).await?;
    let name = q.get("name").ok_or_else(|| ApiError::bad_request("缺少 name 参数"))?;
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(ApiError::bad_request("名称不合法"));
    }
    let p = StdPath::new(&state.config.read().await.data_dir)
        .join("tmp")
        .join(name);
    if !p.exists() {
        return Err(ApiError::not_found("归档不存在（可能已过期）"));
    }
    let file = tokio::fs::File::open(&p).await?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Response::new(axum::body::Body::from_stream(stream));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/gzip"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")).unwrap(),
    );
    Ok(resp)
}

#[derive(Deserialize)]
pub struct PathReq {
    pub path: String,
}

/// 解压归档（zip / mrpack / tar.gz / tar）到所在目录，带路径穿越与 zip bomb 防护
pub async fn files_extract(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PathReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let p = files::safe_join(&rt.dir, &req.path)?;
    if !p.is_file() {
        return Err(ApiError::bad_request("文件不存在"));
    }
    let parent = p
        .parent()
        .ok_or_else(|| ApiError::bad_request("无法确定目标目录"))?
        .to_path_buf();
    let lower = p.to_string_lossy().to_lowercase();
    let f = std::fs::File::open(&p).map_err(|e| ApiError::internal(e.to_string()))?;
    if lower.ends_with(".zip") || lower.ends_with(".mrpack") {
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(f))
            .map_err(|e| ApiError::internal(format!("读取 zip 失败: {e}")))?;
        let mut total: u64 = 0;
        for i in 0..archive.len() {
            let e = archive.by_index(i).map_err(|err| ApiError::internal(err.to_string()))?;
            total += e.size();
            if total > 8 * 1024 * 1024 * 1024 {
                return Err(ApiError::bad_request("解压总量超过 8GB 上限（疑似 zip 炸弹）"));
            }
        }
        for i in 0..archive.len() {
            let mut e = archive.by_index(i).map_err(|err| ApiError::internal(err.to_string()))?;
            let Some(rel) = e.enclosed_name() else { continue };
            let out = parent.join(rel);
            if e.is_dir() {
                std::fs::create_dir_all(&out).map_err(|err| ApiError::internal(err.to_string()))?;
                continue;
            }
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|err| ApiError::internal(err.to_string()))?;
            }
            let mut out_f = std::fs::File::create(&out).map_err(|err| ApiError::internal(err.to_string()))?;
            std::io::copy(&mut e, &mut out_f).map_err(|err| ApiError::internal(err.to_string()))?;
        }
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".tar") {
        let raw: Box<dyn std::io::Read> = if lower.ends_with(".tar") {
            Box::new(f)
        } else {
            Box::new(flate2::read::GzDecoder::new(f))
        };
        let mut archive = tar::Archive::new(raw);
        let mut total: u64 = 0;
        for e in archive
            .entries()
            .map_err(|e| ApiError::internal(format!("读取归档失败: {e}")))?
            .flatten()
        {
            let mut e = e;
            let rel = e
                .path()
                .map_err(|err| ApiError::internal(err.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            if rel.contains("..") || StdPath::new(&rel).is_absolute() {
                return Err(ApiError::bad_request("归档包含不安全路径，已拒绝解压"));
            }
            total += e.size();
            if total > 8 * 1024 * 1024 * 1024 {
                return Err(ApiError::bad_request("解压总量超过 8GB 上限"));
            }
            e.unpack_in(&parent)
                .map_err(|err| ApiError::internal(format!("解压失败: {err}")))?;
        }
    } else {
        return Err(ApiError::bad_request("仅支持 zip / mrpack / tar.gz / tar"));
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- 世界管理 ----------

fn current_level(instance_dir: &StdPath) -> String {
    crate::instance::properties::read(instance_dir)
        .entries
        .iter()
        .find(|e| e.key.as_deref() == Some("level-name"))
        .map(|e| e.value.clone())
        .unwrap_or_else(|| "world".into())
}

pub async fn worlds_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let current = current_level(&rt.dir);
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&rt.dir) {
        for e in rd.flatten() {
            if !e.path().is_dir() || !e.path().join("level.dat").exists() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let mut size = 0u64;
            for f in walkdir::WalkDir::new(e.path()).into_iter().flatten() {
                if let Ok(md) = f.metadata() {
                    if md.is_file() {
                        size += md.len();
                    }
                }
            }
            out.push(json!({
                "name": name,
                "size": size,
                "current": name == current,
            }));
        }
    }
    Ok(Json(json!({ "worlds": out, "running": *rt.status.lock().await != crate::instance::Status::Stopped })))
}

pub async fn worlds_switch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PathReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err(ApiError::bad_request("切换世界前请先停止实例"));
    }
    if req.path.contains("..") || req.path.contains('/') || req.path.contains('\\') {
        return Err(ApiError::bad_request("世界名不合法"));
    }
    properties::set_values(&rt.dir, &[(("level-name").to_string(), req.path.clone())]).await?;
    Ok(Json(json!({ "ok": true, "world": req.path })))
}

#[derive(Deserialize)]
pub struct WorldCreateReq {
    pub name: String,
    pub seed: Option<String>,
}

pub async fn worlds_create(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<WorldCreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err(ApiError::bad_request("创建世界前请先停止实例"));
    }
    let name = req.name.trim().to_string();
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains(' ')
    {
        return Err(ApiError::bad_request("世界名不合法（不能含空格与路径符号）"));
    }
    let mut kv = vec![(("level-name").to_string(), name.clone())];
    if let Some(seed) = req.seed.as_deref().filter(|s| !s.trim().is_empty()) {
        kv.push((("level-seed").to_string(), seed.trim().to_string()));
    }
    properties::set_values(&rt.dir, &kv).await?;
    Ok(Json(json!({ "ok": true, "world": name })))
}

pub async fn worlds_delete(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != crate::instance::Status::Stopped {
        return Err(ApiError::bad_request("删除世界前请先停止实例"));
    }
    let current = current_level(&rt.dir);
    if name == current {
        return Err(ApiError::bad_request("不能删除正在使用的世界"));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(ApiError::bad_request("世界名不合法"));
    }
    let dir = rt.dir.join(&name);
    if !dir.join("level.dat").exists() {
        return Err(ApiError::not_found("世界不存在"));
    }
    tokio::fs::remove_dir_all(&dir)
        .await
        .map_err(|e| ApiError::internal(format!("删除失败: {e}")))?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- 计划任务 ----------

pub async fn tasks_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.clone();
    let tasks = crate::instance::tasks::load(&dir);
    Ok(Json(json!({ "tasks": tasks })))
}

#[derive(Deserialize)]
pub struct TaskCreateReq {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub value: String,
    pub interval_mins: u64,
}

pub async fn tasks_create(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut task): Json<crate::instance::tasks::Task>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.clone();
    task.id = uuid::Uuid::new_v4().to_string();
    task.name = task.name.trim().to_string();
    if task.name.is_empty() {
        return Err(ApiError::bad_request("任务名称不能为空"));
    }
    if !matches!(task.kind.as_str(), "command" | "backup" | "restart") {
        return Err(ApiError::bad_request("任务类型不合法"));
    }
    if task.kind == "command" && task.value.trim().is_empty() {
        return Err(ApiError::bad_request("命令不能为空"));
    }
    if task.interval_mins == 0 {
        return Err(ApiError::bad_request("间隔必须大于 0 分钟"));
    }
    let mut tasks = crate::instance::tasks::load(&dir);
    tasks.push(task);
    crate::instance::tasks::save(&dir, &tasks).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct TaskUpdateReq {
    pub id: String,
    /// enable / disable / delete / run
    pub op: String,
}

pub async fn tasks_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<TaskUpdateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.clone();
    let mut tasks = crate::instance::tasks::load(&dir);
    match req.op.as_str() {
        "run" => {
            if let Some(t) = tasks.iter().find(|t| t.id == req.id) {
                let st2 = state.clone();
                let rt2 = rt.clone();
                let t2 = t.clone();
                tokio::spawn(async move {
                    crate::instance::tasks::run_task_now(&st2, &rt2, &t2.id).await;
                });
            }
            return Ok(Json(json!({ "ok": true })));
        }
        _ => {}
    }
    let Some(t) = tasks.iter_mut().find(|t| t.id == req.id) else {
        return Err(ApiError::not_found("任务不存在"));
    };
    match req.op.as_str() {
        "enable" => t.enabled = true,
        "disable" => t.enabled = false,
        "delete" => {
            tasks.retain(|x| x.id != req.id);
        }
        _ => return Err(ApiError::bad_request("未知操作")),
    }
    crate::instance::tasks::save(&dir, &tasks).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- 常见配置文件直达 ----------

const KNOWN_CONFIGS: &[&str] = &[
    "server.properties",
    "eula.txt",
    "bukkit.yml",
    "spigot.yml",
    "paper.yml",
    "config/paper-global.yml",
    "config/paper-world-defaults.yml",
    "commands.yml",
    "permissions.yml",
    "velocity.toml",
    "whitelist.json",
    "ops.json",
    "banned-players.json",
    "banned-ips.json",
];

pub async fn configs_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let mut out: Vec<String> = Vec::new();
    for c in KNOWN_CONFIGS {
        if rt.dir.join(c).exists() {
            out.push((*c).to_string());
        }
    }
    if let Ok(rd) = std::fs::read_dir(rt.dir.join("config")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.to_lowercase().ends_with(".toml") {
                out.push(format!("config/{name}"));
            }
        }
    }
    Ok(Json(json!({ "configs": out })))
}
