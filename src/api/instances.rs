use crate::error::{ApiError, ApiResult};
use crate::instance::modpack;
use crate::instance::process;
use crate::instance::{get_instance, InstanceMeta, InstanceRuntime, Status};
use crate::jobs::{finish_job, Job};
use crate::state::AppState;
use crate::util::now_str;
use axum::extract::{Multipart, Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;

// ---------- 实例列表 / 详情 ----------

pub async fn list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let map = state.instances.read().await;
    let mut items = Vec::new();
    for rt in map.values() {
        items.push(rt.summary().await);
    }
    drop(map);
    items.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.name.cmp(&b.name)));
    Json(json!({ "instances": items }))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
    /// 原版：下载官方服务端的 MC 版本；模组服：游戏版本
    pub mc_version: Option<String>,
    /// 模组加载器：fabric / quilt / forge / neoforge
    pub mod_loader: Option<String>,
    /// 模组加载器版本
    pub loader_version: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("实例名称不能为空"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let dir = state.config.read().await.instances_dir().join(&id);
    tokio::fs::create_dir_all(&dir).await?;
    let mc_version = req
        .mc_version
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let mod_loader = req
        .mod_loader
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let loader_version = req
        .loader_version
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if mod_loader.is_some() && (mc_version.is_none() || loader_version.is_none()) {
        return Err(ApiError::bad_request("创建模组服需要同时选择 MC 版本和加载器版本"));
    }
    let meta = InstanceMeta {
        id: id.clone(),
        name,
        created_at: now_str(),
        java_path: None,
        min_ram_mb: 1024,
        max_ram_mb: 4096,
        jar: None,
        jvm_args: String::new(),
        auto_restart: false,
        auto_start_on_boot: false,
        mc_version: mc_version.clone(),
        mod_loader: mod_loader.clone(),
    };
    tokio::fs::write(dir.join("instance.json"), serde_json::to_string_pretty(&meta)?).await?;
    let rt = InstanceRuntime::new(meta, dir);
    state.instances.write().await.insert(id.clone(), rt);

    let job_id = uuid::Uuid::new_v4().to_string();
    if let Some(loader) = mod_loader {
        // 模组服安装任务
        let game = mc_version.expect("上面已校验");
        let lver = loader_version.expect("上面已校验");
        state
            .jobs
            .lock()
            .unwrap()
            .insert(job_id.clone(), Job::new(job_id.clone()));
        let st2 = state.clone();
        let jid = job_id.clone();
        let iid = id.clone();
        tokio::spawn(async move {
            crate::instance::loaders::install(&st2, &jid, &iid, &loader, &game, &lver).await;
        });
        return Ok(Json(json!({ "id": id, "job_id": job_id })));
    }
    if let Some(ver) = mc_version {
        // 原版官方服务端下载任务
        state
            .jobs
            .lock()
            .unwrap()
            .insert(job_id.clone(), Job::new(job_id.clone()));
        let st2 = state.clone();
        let jid = job_id.clone();
        let iid = id.clone();
        tokio::spawn(async move {
            crate::instance::vanilla::download_server(&st2, &jid, &iid, &ver).await;
        });
        return Ok(Json(json!({ "id": id, "job_id": job_id })));
    }
    Ok(Json(json!({ "id": id })))
}

/// Minecraft 版本清单（官方源，自动镜像回退，缓存 10 分钟）
pub async fn versions(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let manifest = crate::instance::vanilla::fetch_manifest(&state)
        .await
        .map_err(ApiError::internal)?;
    let mut list = Vec::new();
    if let Some(arr) = manifest.get("versions").and_then(|v| v.as_array()) {
        for e in arr {
            list.push(json!({
                "id": e.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "type": e.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                "release_time": e.get("releaseTime").and_then(|v| v.as_str()).unwrap_or(""),
            }));
        }
    }
    let latest = manifest.get("latest").cloned().unwrap_or(json!({}));
    Ok(Json(json!({ "latest": latest, "versions": list })))
}

/// 模组加载器可用的 MC 版本列表
pub async fn loader_game_versions(
    State(state): State<AppState>,
    Path(loader): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let list = crate::instance::loaders::game_versions(&state, &loader)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "versions": list })))
}

/// 模组加载器在指定 MC 版本下的加载器版本列表
pub async fn loader_versions(
    State(state): State<AppState>,
    Path(loader): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let game = q.get("game").cloned().unwrap_or_default();
    let list = crate::instance::loaders::loader_versions(&state, &loader, &game)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "versions": list })))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    Ok(Json(serde_json::to_value(rt.summary().await)?))
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub name: Option<String>,
    pub java_path: Option<String>,
    pub min_ram_mb: Option<u32>,
    pub max_ram_mb: Option<u32>,
    pub jar: Option<String>,
    pub jvm_args: Option<String>,
    pub auto_restart: Option<bool>,
    pub auto_start_on_boot: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    {
        let mut meta = rt.meta.write().await;
        if let Some(n) = req.name {
            let n = n.trim().to_string();
            if !n.is_empty() {
                meta.name = n;
            }
        }
        if let Some(j) = req.java_path {
            meta.java_path = if j.trim().is_empty() { None } else { Some(j.trim().to_string()) };
        }
        if let Some(v) = req.min_ram_mb {
            meta.min_ram_mb = v;
        }
        if let Some(v) = req.max_ram_mb {
            meta.max_ram_mb = v;
        }
        if let Some(v) = req.jar {
            meta.jar = if v.trim().is_empty() { None } else { Some(v.trim().replace('\\', "/")) };
        }
        if let Some(v) = req.jvm_args {
            meta.jvm_args = v;
        }
        if let Some(v) = req.auto_restart {
            meta.auto_restart = v;
        }
        if let Some(v) = req.auto_start_on_boot {
            meta.auto_start_on_boot = v;
        }
        if meta.min_ram_mb > meta.max_ram_mb {
            meta.min_ram_mb = meta.max_ram_mb;
        }
    }
    rt.persist().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != Status::Stopped {
        process::stop(state.clone(), rt.clone()).await?;
    }
    let dir = rt.dir.clone();
    state.instances.write().await.remove(&id);
    tokio::fs::remove_dir_all(&dir)
        .await
        .map_err(|e| ApiError::internal(format!("删除实例目录失败: {e}")))?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- 电源控制 / 控制台命令 ----------

pub async fn start(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    process::start(state.clone(), rt).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    process::stop(state.clone(), rt).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn restart(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    if *rt.status.lock().await != Status::Stopped {
        process::stop(state.clone(), rt.clone()).await?;
    }
    process::start(state.clone(), rt).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct CommandReq {
    pub command: String,
}

pub async fn command(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CommandReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let cmd = req.command.trim().to_string();
    // RCON 优先：能拿到命令输出，写回控制台
    let dir = rt.dir.clone();
    let cmd2 = cmd.clone();
    let rcon_out = tokio::task::spawn_blocking(move || {
        let Some((addr, pass, _)) = crate::rcon::rcon_config(&dir) else { return None };
        let mut c = crate::rcon::RconClient::connect(&addr, &pass).ok()?;
        c.command(&cmd2).ok()
    })
    .await
    .unwrap_or(None);
    if let Some(out) = rcon_out {
        for line in out.lines() {
            crate::instance::process::push_log(&rt, format!("[RCON] {line}")).await;
        }
        return Ok(Json(json!({ "ok": true, "mode": "rcon" })));
    }
    process::send_command(&rt, &cmd).await?;
    Ok(Json(json!({ "ok": true, "mode": "stdin" })))
}

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    Ok(Json(serde_json::to_value(rt.summary().await)?))
}

pub async fn accept_eula(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    crate::instance::accept_eula_sync(&rt.dir)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn open_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.clone();
    let res: Result<(), String> = {
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("explorer")
                .arg(&dir)
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::process::Command::new("xdg-open")
                .arg(&dir)
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    };
    res.map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- 全局统计 ----------

fn dir_size(dir: &std::path::Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

pub async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut pid_map: Vec<(String, u32)> = Vec::new();
    let mut items = Vec::new();
    let instance_ids: Vec<String> = state.instances.read().await.keys().cloned().collect();
    {
        let map = state.instances.read().await;
        for rt in map.values() {
            let s = rt.summary().await;
            pid_map.push((s.id.clone(), rt.pid.load(Ordering::SeqCst)));
            items.push(s);
        }
    }
    items.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.name.cmp(&b.name)));

    let mut per_instance = serde_json::Map::new();
    let (cpu, mem_total, mem_used) = {
        let mut sys = state.sys.lock().unwrap();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let pids: Vec<sysinfo::Pid> = pid_map
            .iter()
            .filter(|(_, p)| *p != 0)
            .map(|(_, p)| sysinfo::Pid::from_u32(*p))
            .collect();
        if !pids.is_empty() {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
        }
        for (id, pid) in &pid_map {
            let (c, m) = if *pid == 0 {
                (0.0_f32, 0.0_f64)
            } else {
                match sys.process(sysinfo::Pid::from_u32(*pid)) {
                    Some(p) => (p.cpu_usage(), p.memory() as f64 / 1048576.0),
                    None => (0.0, 0.0),
                }
            };
            per_instance.insert(id.clone(), json!({ "cpu": c, "mem_mb": m }));
        }
        (sys.global_cpu_usage(), sys.total_memory(), sys.used_memory())
    };

    // 磁盘用量与实例空间排行（可观测性）
    let data_dir = state.config.read().await.data_dir.clone();
    let (disk_total, disk_free) = {
        let dd = data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let total = fs4::total_space(&dd).unwrap_or(0);
            let free = fs4::available_space(&dd).unwrap_or(0);
            (total, free)
        })
        .await
        .unwrap_or((0, 0))
    };
    let mut sizes = serde_json::Map::new();
    for id in &instance_ids {
        let cached = state.size_cache.lock().unwrap().get(id).cloned();
        let bytes = match cached {
            Some((at, bytes)) if at.elapsed().as_secs() < 300 => bytes,
            _ => {
                let dir = std::path::Path::new(&data_dir).join("instances").join(id);
                let bytes = tokio::task::spawn_blocking(move || dir_size(&dir))
                    .await
                    .unwrap_or(0);
                state.size_cache.lock().unwrap().insert(id.clone(), (std::time::Instant::now(), bytes));
                bytes
            }
        };
        sizes.insert(id.clone(), json!(bytes));
    }
    Json(json!({
        "cpu_usage": cpu,
        "mem_total": mem_total,
        "mem_used": mem_used,
        "instances": items,
        "per_instance": per_instance,
        "disk": { "total": disk_total, "free": disk_free },
        "sizes": sizes,
    }))
}

// ---------- 导入 ----------

#[derive(Deserialize)]
pub struct ImportPathReq {
    pub path: String,
    pub name: Option<String>,
}

pub async fn import_path(
    State(state): State<AppState>,
    Json(req): Json<ImportPathReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let path = PathBuf::from(req.path.trim());
    if !path.is_dir() {
        return Err(ApiError::bad_request("目录不存在或不为文件夹"));
    }
    let name = req
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "导入的实例".into())
        });
    let instances_dir = state.config.read().await.instances_dir();
    let job_id = uuid::Uuid::new_v4().to_string();
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), Job::new(job_id.clone()));
    let st2 = state.clone();
    let jid = job_id.clone();
    tokio::task::spawn_blocking(move || {
        let r = modpack::import_from_dir(&st2, &jid, &instances_dir, &path, &name);
        match r {
            Ok(instance_id) => finish_job(&st2, &jid, None, Some(instance_id)),
            Err(e) => finish_job(&st2, &jid, Some(e), None),
        }
    });
    Ok(Json(json!({ "job_id": job_id })))
}

pub async fn import_upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let mut name: Option<String> = None;
    let mut zip_path: Option<PathBuf> = None;
    let mut zip_name = String::from("导入的实例");

    while let Some(mut field) = multipart.next_field().await? {
        match field.name().unwrap_or("") {
            "name" => name = Some(field.text().await?),
            "file" => {
                if let Some(fname) = field.file_name() {
                    if let Some(stem) = std::path::Path::new(fname).file_stem() {
                        let s = stem.to_string_lossy().to_string();
                        if !s.trim().is_empty() {
                            zip_name = s;
                        }
                    }
                }
                let tmp = std::env::temp_dir().join(format!("mcspr_import_{}.zip", uuid::Uuid::new_v4()));
                let mut f = tokio::fs::File::create(&tmp).await?;
                while let Some(chunk) = field.chunk().await? {
                    f.write_all(&chunk).await?;
                }
                f.flush().await?;
                drop(f);
                zip_path = Some(tmp);
            }
            _ => {}
        }
    }

    let Some(zip_path) = zip_path else {
        return Err(ApiError::bad_request("请上传 .zip 整合包文件"));
    };
    let name = name.filter(|s| !s.trim().is_empty()).unwrap_or(zip_name);

    let instances_dir = state.config.read().await.instances_dir();
    let job_id = uuid::Uuid::new_v4().to_string();
    state
        .jobs
        .lock()
        .unwrap()
        .insert(job_id.clone(), Job::new(job_id.clone()));
    let st2 = state.clone();
    let jid = job_id.clone();
    tokio::task::spawn_blocking(move || {
        let r = modpack::import_from_zip(&st2, &jid, &instances_dir, &zip_path, &name);
        match r {
            Ok(instance_id) => finish_job(&st2, &jid, None, Some(instance_id)),
            Err(e) => finish_job(&st2, &jid, Some(e), None),
        }
    });
    Ok(Json(json!({ "job_id": job_id })))
}

pub async fn get_job(
    State(state): State<AppState>,
    Path(jid): Path<String>,
) -> ApiResult<Json<Job>> {
    let job = state
        .jobs
        .lock()
        .unwrap()
        .get(&jid)
        .cloned()
        .ok_or_else(|| ApiError::not_found("任务不存在"))?;
    Ok(Json(job))
}
