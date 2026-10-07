use crate::error::{ApiError, ApiResult};
use crate::instance::{game_backup, get_instance, process, InstanceRuntime, Status};
use crate::jobs::{finish_job, log_job, Job};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::Json;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub async fn list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.clone();
    let (provider, backups) = tokio::task::spawn_blocking(move || {
        let provider = game_backup::detect(&dir)?;
        let backups = match &provider {
            Some(p) => game_backup::list(&dir, p)?,
            None => Vec::new(),
        };
        Ok::<_, String>((provider, backups))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::bad_request)?;
    let (active_job, last_job) = {
        let jobs = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        let matching = |job: &&Job| {
            job.kind.as_deref() == Some("game-backup")
                && job.instance_id.as_deref() == Some(id.as_str())
        };
        let active = jobs
            .values()
            .filter(matching)
            .find(|j| j.status == "running")
            .map(|j| j.id.clone());
        let latest = jobs
            .values()
            .filter(matching)
            .max_by_key(|j| &j.created_at)
            .map(|j| j.id.clone());
        (active, latest)
    };
    Ok(Json(json!({
        "provider": provider,
        "backups": backups,
        "status": rt.display_status().await,
        "active_job": active_job,
        "last_job": last_job,
    })))
}

async fn provider(rt: &Arc<InstanceRuntime>) -> ApiResult<game_backup::Provider> {
    let dir = rt.dir.clone();
    tokio::task::spawn_blocking(move || game_backup::detect(&dir))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?
        .ok_or_else(|| ApiError::bad_request("未识别到支持的游戏内备份模组（ServerUtilities）"))
}

fn new_job(state: &AppState, id: &str, message: &str) -> String {
    let jid = uuid::Uuid::new_v4().to_string();
    let mut job = Job::new(jid.clone());
    job.kind = Some("game-backup".into());
    job.instance_id = Some(id.into());
    job.log(message);
    state
        .jobs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(jid.clone(), job);
    jid
}

pub async fn create(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let busy = state
        .busy_guard(&id)
        .ok_or_else(|| ApiError::bad_request("该实例有整体操作正在进行，请稍候"))?;
    if rt.display_status().await != "running" {
        return Err(ApiError::bad_request(
            "游戏内备份需要实例已完成启动并正在运行",
        ));
    }
    let p = provider(&rt).await?;
    if !p.enabled || !p.command_enabled {
        return Err(ApiError::bad_request(
            "ServerUtilities 备份或 backup 命令已禁用",
        ));
    }
    let name = format!(
        "panel-{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4().simple()
    );
    let jid = new_job(&state, &id, "正在通过 ServerUtilities 创建游戏内备份");
    let st = state.clone();
    let job_id = jid.clone();
    tokio::spawn(async move {
        let _busy = busy;
        let result = run_create(&st, &job_id, &rt, p, &name).await;
        finish_job(&st, &job_id, result.err(), Some(id));
    });
    Ok(Json(json!({"job_id": jid})))
}

async fn run_create(
    state: &AppState,
    jid: &str,
    rt: &Arc<InstanceRuntime>,
    p: game_backup::Provider,
    name: &str,
) -> Result<(), String> {
    let dir = rt.dir.clone();
    let check_dir = dir.clone();
    let check_provider = p.clone();
    let out_dir =
        tokio::task::spawn_blocking(move || game_backup::backup_dir(&check_dir, &check_provider))
            .await
            .map_err(|e| e.to_string())??;
    let filename = format!("{name}.zip");
    let output = out_dir.join(&filename);
    let mut cursor = rt.next_seq.load(Ordering::SeqCst);
    process::send_command(rt, &format!("backup start {name}"))
        .await
        .map_err(|e| e.to_string())?;
    log_job(
        state,
        jid,
        format!("已发送 backup start；等待 {filename} 完成"),
    );
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    loop {
        if *rt.status.lock().await == Status::Stopped {
            return Err("实例已停止，游戏内备份中断".into());
        }
        let lines: Vec<_> = rt
            .log_buf
            .lock()
            .await
            .iter()
            .filter(|line| line.seq > cursor)
            .cloned()
            .collect();
        for line in &lines {
            cursor = cursor.max(line.seq);
            let lower = line.line.to_lowercase();
            if lower.contains("error while backing up")
                || lower.contains("backup cancelled")
                || lower.contains("backup already running")
                || lower.contains("backup is already running")
                || lower.contains("unknown command")
                || lower.contains("couldn't create backup")
            {
                return Err(format!("ServerUtilities: {}", line.line));
            }
            if lower.contains("backing up ") || lower.contains("backup done in") {
                log_job(state, jid, &line.line);
            }
        }
        if output.is_file() {
            let check_dir = dir.clone();
            let check_provider = p.clone();
            let check_name = filename.clone();
            let preview = tokio::task::spawn_blocking(move || {
                game_backup::preview(&check_dir, &check_provider, &check_name)
            })
            .await
            .map_err(|e| e.to_string())??;
            log_job(
                state,
                jid,
                format!(
                    "备份完成：{filename}，{} 个文件，{} 字节",
                    preview.files, preview.total_size
                ),
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("等待游戏内备份超时，请检查控制台和 ServerUtilities 的备份目录".into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub async fn download(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let rt = get_instance(&state, &id).await?;
    let p = provider(&rt).await?;
    let dir = rt.dir.clone();
    let filename = name.clone();
    let path = tokio::task::spawn_blocking(move || game_backup::backup_file(&dir, &p, &filename))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    let file = tokio::fs::File::open(path).await?;
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        super::backup::safe_disposition(&name),
    );
    Ok(response)
}

pub async fn preview(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let p = provider(&rt).await?;
    let dir = rt.dir.clone();
    let preview = tokio::task::spawn_blocking(move || game_backup::preview(&dir, &p, &name))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({"preview": preview})))
}
pub async fn delete(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let p = provider(&rt).await?;
    let dir = rt.dir.clone();
    tokio::task::spawn_blocking(move || game_backup::delete(&dir, &p, &name))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "ok": true })))
}
pub async fn update_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<game_backup::BackupConfigUpdate>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let dir = rt.dir.clone();
    let provider = tokio::task::spawn_blocking(move || {
        game_backup::update_config(&dir, &req)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "ok": true, "provider": provider })))
}

pub async fn restore(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let busy = state
        .busy_guard(&id)
        .ok_or_else(|| ApiError::bad_request("该实例有整体操作正在进行，请稍候"))?;
    if *rt.status.lock().await != Status::Stopped {
        return Err(ApiError::bad_request("还原游戏内备份前请先停止实例"));
    }
    let p = provider(&rt).await?;
    let dir = rt.dir.clone();
    let jid = new_job(&state, &id, "正在校验游戏内备份并保留当前数据");
    let st = state.clone();
    let job_id = jid.clone();
    tokio::spawn(async move {
        let worker_state = st.clone();
        let worker_jid = job_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _busy = busy;
            let restored = game_backup::restore(&dir, &p, &name)?;
            log_job(
                &worker_state,
                &worker_jid,
                format!(
                    "还原完成。恢复前数据已保留于 {}；请手动启动实例。",
                    restored.recovery_directory,
                ),
            );
            Ok::<_, String>(())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        finish_job(&st, &job_id, result.err(), Some(id));
    });
    Ok(Json(json!({"job_id": jid})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelConfig;
    use crate::instance::InstanceMeta;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use zip::{write::SimpleFileOptions, ZipWriter};

    struct Fixture {
        dir: PathBuf,
        state: AppState,
        rt: Arc<InstanceRuntime>,
    }
    impl Fixture {
        async fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("mcspr-game-api-test-{}", uuid::Uuid::new_v4()));
            let state = AppState::new(PanelConfig {
                data_dir: dir.to_string_lossy().into_owned(),
                token: "unit-test-secret".into(),
                ..PanelConfig::default()
            })
            .await
            .unwrap();
            let instance = dir.join("instances/test");
            fs::create_dir_all(instance.join("mods")).unwrap();
            fs::create_dir_all(instance.join("serverutilities")).unwrap();
            fs::create_dir_all(instance.join("backups")).unwrap();
            fs::create_dir_all(instance.join("World")).unwrap();
            fs::write(instance.join("server.properties"), "level-name=World\n").unwrap();
            fs::write(instance.join("serverutilities/serverutilities.cfg"), "backups {\n B:enable_backups=true\n S:backup_folder_path=./backups/\n}\ncommands {\n B:backup=true\n}\n").unwrap();
            fs::write(instance.join("World/level.dat"), b"current").unwrap();
            let mut jar = ZipWriter::new(
                fs::File::create(instance.join("mods/ServerUtilities.jar")).unwrap(),
            );
            jar.start_file("mcmod.info", SimpleFileOptions::default())
                .unwrap();
            jar.write_all(br#"[{"modid":"serverutilities","version":"2.4.14"}]"#)
                .unwrap();
            jar.finish().unwrap();
            let mut zip =
                ZipWriter::new(fs::File::create(instance.join("backups/old.zip")).unwrap());
            zip.start_file("World/level.dat", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"old world").unwrap();
            zip.finish().unwrap();
            let meta: InstanceMeta =
                serde_json::from_value(json!({"id":"test", "name":"test"})).unwrap();
            let rt = InstanceRuntime::new(meta, instance);
            state
                .instances
                .write()
                .await
                .insert("test".into(), rt.clone());
            Self { dir, state, rt }
        }
        async fn wait_job(&self, jid: &str) -> Job {
            for _ in 0..100 {
                let job = self.state.jobs.lock().unwrap().get(jid).unwrap().clone();
                if job.status != "running" {
                    return job;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("job did not finish");
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    #[tokio::test]
    async fn list_retains_latest_completed_job_and_separates_active_job() {
        let f = Fixture::new().await;
        let old = new_job(&f.state, "test", "old");
        finish_job(&f.state, &old, None, Some("test".into()));
        tokio::time::sleep(Duration::from_millis(2)).await;
        let latest = new_job(&f.state, "test", "new");
        let response = list(State(f.state.clone()), Path("test".into()))
            .await
            .unwrap()
            .0;
        assert_eq!(response["active_job"], latest);
        assert_eq!(response["last_job"], latest);
        finish_job(
            &f.state,
            &latest,
            Some("failed".into()),
            Some("test".into()),
        );
        let response = list(State(f.state.clone()), Path("test".into()))
            .await
            .unwrap()
            .0;
        assert!(response["active_job"].is_null());
        assert_eq!(response["last_job"], latest);
    }
    #[tokio::test]
    async fn restore_blocks_running_and_start_during_busy_then_releases_lock() {
        let f = Fixture::new().await;
        *f.rt.status.lock().await = Status::Running;
        assert!(restore(
            State(f.state.clone()),
            Path(("test".into(), "old.zip".into()))
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("停止"));
        let guard = f.state.busy_guard("test").unwrap();
        *f.rt.status.lock().await = Status::Stopped;
        assert!(process::start(f.state.clone(), f.rt.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("整体操作"));
        assert!(restore(
            State(f.state.clone()),
            Path(("test".into(), "old.zip".into()))
        )
        .await
        .is_err());
        drop(guard);
        let response = restore(
            State(f.state.clone()),
            Path(("test".into(), "old.zip".into())),
        )
        .await
        .unwrap()
        .0;
        let job = f.wait_job(response["job_id"].as_str().unwrap()).await;
        assert_eq!(job.status, "done", "{:?}", job.logs);
        assert_eq!(
            fs::read(f.rt.dir.join("World/level.dat")).unwrap(),
            b"old world"
        );
        assert!(f.state.busy_guard("test").is_some());
        let response = restore(
            State(f.state.clone()),
            Path(("test".into(), "missing.zip".into())),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            f.wait_job(response["job_id"].as_str().unwrap())
                .await
                .status,
            "error"
        );
        assert!(f.state.busy_guard("test").is_some());
    }

    #[tokio::test]
    async fn create_state_and_command_errors_are_explicit_and_release_lock() {
        let f = Fixture::new().await;
        assert!(create(State(f.state.clone()), Path("test".into()))
            .await
            .unwrap_err()
            .to_string()
            .contains("运行"));
        *f.rt.status.lock().await = Status::Running;
        let response = create(State(f.state.clone()), Path("test".into()))
            .await
            .unwrap()
            .0;
        let job = f.wait_job(response["job_id"].as_str().unwrap()).await;
        assert_eq!(job.status, "error");
        assert!(f.state.busy_guard("test").is_some());
        fs::write(
            f.rt.dir.join("serverutilities/serverutilities.cfg"),
            "backups {\n B:enable_backups=false\n}\n",
        )
        .unwrap();
        assert!(create(State(f.state.clone()), Path("test".into()))
            .await
            .unwrap_err()
            .to_string()
            .contains("禁用"));
    }

    #[tokio::test]
    async fn authenticated_http_list_preview_download_and_unsupported_state() {
        let f = Fixture::new().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://{}/api/instances/test/game-backups",
            listener.local_addr().unwrap()
        );
        let app = crate::api::router(f.state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        assert_eq!(client.get(&base).send().await.unwrap().status(), 401);
        let response: serde_json::Value = client
            .get(&base)
            .bearer_auth("unit-test-secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["provider"]["version"], "2.4.14");
        assert_eq!(response["backups"][0]["name"], "old.zip");
        let response: serde_json::Value = client
            .get(format!("{base}/old.zip/preview"))
            .bearer_auth("unit-test-secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["preview"]["world"], "World");
        let response = client
            .get(format!("{base}/old.zip"))
            .bearer_auth("unit-test-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["content-type"], "application/zip");
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            fs::read(f.rt.dir.join("backups/old.zip")).unwrap()
        );
        let response = client
            .delete(format!("{base}/old.zip"))
            .bearer_auth("unit-test-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(!f.rt.dir.join("backups/old.zip").exists());
        let update_res: serde_json::Value = client
            .post(format!("{base}/config"))
            .bearer_auth("unit-test-secret")
            .json(&serde_json::json!({
                "enabled": false,
                "interval_hours": 2.5,
                "keep": 15
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(update_res["provider"]["enabled"], false);
        assert_eq!(update_res["provider"]["interval_hours"], 2.5);
        assert_eq!(update_res["provider"]["keep"], 15);
        fs::rename(
            f.rt.dir.join("mods/ServerUtilities.jar"),
            f.rt.dir.join("mods/ServerUtilities.jar.disabled"),
        )
        .unwrap();
        let response: serde_json::Value = client
            .get(&base)
            .bearer_auth("unit-test-secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response["provider"].is_null());
        assert_eq!(response["backups"], json!([]));
        server.abort();
        let _ = server.await;
    }
}
