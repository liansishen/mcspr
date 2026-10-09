//! 统一任务中心：任务元数据、有界持久化与崩溃恢复。
//!
//! 任务同时存在于内存映射（`AppState::jobs`）与 `data/tasks/jobs.json`。
//! 面板重启后把仍在 `running` 的任务标记为 `interrupted`，不做盲目重放。

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;

/// 单条任务保留的最大日志行数
pub const MAX_LOGS: usize = 400;
/// 终态任务保留上限
pub const MAX_TERMINAL_JOBS: usize = 1000;
/// 运行中任务保留上限
pub const MAX_ACTIVE_JOBS: usize = 256;
/// 终态任务保留天数
pub const RETENTION_DAYS: i64 = 7;
/// 运行中任务日志落盘节流间隔（毫秒）
const PERSIST_THROTTLE_MS: u64 = 1000;

fn default_stage() -> String {
    "running".into()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// running | done | error | interrupted
    pub status: String,
    /// 0-100，仅下载类任务使用
    pub progress: u8,
    pub logs: Vec<String>,
    pub instance_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default = "default_stage")]
    pub stage: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Job {
    pub fn new(id: String) -> Self {
        Self {
            id,
            status: "running".into(),
            progress: 0,
            logs: Vec::new(),
            instance_id: None,
            kind: None,
            title: None,
            user_id: None,
            operation_id: None,
            stage: "running".into(),
            created_at: now_iso(),
            updated_at: None,
            finished_at: None,
            result: None,
            error: None,
        }
    }

    pub fn log(&mut self, msg: impl Into<String>) {
        let line = format!(
            "[{}] {}",
            chrono::Local::now().format("%H:%M:%S"),
            msg.into()
        );
        self.logs.push(line);
        if self.logs.len() > MAX_LOGS {
            self.logs.drain(0..self.logs.len() - MAX_LOGS);
        }
        self.updated_at = Some(now_iso());
    }
}

/// 创建任务时的元数据：类型、标题、实例与发起人。
#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub kind: String,
    pub title: String,
    pub instance_id: Option<String>,
    pub user_id: Option<String>,
    pub operation_id: Option<String>,
}

/// 登记并持久化一个新任务，返回任务编号。
pub fn create_job(state: &AppState, spec: NewJob) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let mut job = Job::new(id.clone());
    job.kind = Some(spec.kind).filter(|k| !k.is_empty());
    job.title = Some(spec.title).filter(|t| !t.is_empty());
    job.instance_id = spec.instance_id;
    job.user_id = spec.user_id;
    job.operation_id = spec.operation_id;
    state
        .jobs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.clone(), job);
    persist_jobs(state, true);
    id
}

pub fn log_job(state: &AppState, id: &str, msg: impl Into<String>) {
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = map.get_mut(id) {
            j.log(msg);
        }
    }
    persist_jobs(state, false);
}

pub fn set_progress(state: &AppState, id: &str, pct: u8) {
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = map.get_mut(id) {
            j.progress = pct.min(100);
            j.updated_at = Some(now_iso());
        }
    }
    persist_jobs(state, false);
}

pub fn finish_job(state: &AppState, id: &str, err: Option<String>, instance_id: Option<String>) {
    let mut kind: Option<String> = None;
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = map.get_mut(id) {
            kind = j.kind.clone();
            match &err {
                Some(e) => {
                    j.status = "error".into();
                    j.stage = "error".into();
                    j.error = Some(e.clone());
                    j.log(format!("❌ {e}"));
                }
                None => {
                    j.status = "done".into();
                    j.stage = "done".into();
                }
            }
            j.instance_id = instance_id.clone();
            let ts = now_iso();
            j.finished_at = Some(ts.clone());
            j.updated_at = Some(ts);
        }
    }
    persist_jobs(state, true);
    // 实例级后台任务真正完成后唤醒白名单协调；白名单自身任务按 kind 排除，避免循环。
    if instance_id.is_some() && kind.as_deref() != Some("whitelist-sync") {
        crate::api::whitelist_sync::trigger();
    }
}

/// 当前内存中全部任务快照。
pub fn all(state: &AppState) -> Vec<Job> {
    state
        .jobs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect()
}

/// 有界持久化：裁剪历史后原子写入 `data/tasks/jobs.json`（权限 0600）。
pub fn persist_jobs(state: &AppState, force: bool) {
    if !force {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = state.job_persist_at.load(Ordering::Relaxed);
        if now.saturating_sub(last) < PERSIST_THROTTLE_MS {
            return;
        }
        state.job_persist_at.store(now, Ordering::Relaxed);
    }
    let json = {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        prune_jobs(&mut map);
        serde_json::to_string(&*map).unwrap_or_else(|_| "{}".into())
    };
    let _ = write_private(&state.tasks_dir.join("jobs.json"), json.as_bytes());
}

/// 面板启动时加载持久化任务；运行中任务标记为中断，不自动重放。
pub fn restore(state: &AppState) {
    let path = state.tasks_dir.join("jobs.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, Job>>(&text) else {
        return;
    };
    let ts = now_iso();
    let mut target = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
    for (id, mut job) in map {
        if job.status == "running" {
            job.status = "interrupted".into();
            job.stage = "interrupted".into();
            job.error = Some("面板重启，任务已中断，未自动重放".into());
            job.finished_at = Some(ts.clone());
            job.updated_at = Some(ts.clone());
            job.log("⚠ 面板重启，任务中断（不会自动重放，请核实后重试）");
        }
        target.insert(id, job);
    }
    prune_jobs(&mut target);
}

/// 启动时清理上次运行遗留的上传落盘缓存（`upload-*.spool`）。
pub fn cleanup_stale_uploads(state: &AppState) {
    let Ok(rd) = std::fs::read_dir(&state.tasks_dir) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("upload-") && name.ends_with(".spool") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn prune_jobs(map: &mut HashMap<String, Job>) {
    let now = chrono::Utc::now();
    let mut terminal: Vec<(String, chrono::DateTime<chrono::Utc>)> = Vec::new();
    let mut active: Vec<(String, chrono::DateTime<chrono::Utc>)> = Vec::new();
    for (id, job) in map.iter() {
        let ts = chrono::DateTime::parse_from_rfc3339(&job.created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or(now);
        if job.status == "running" {
            active.push((id.clone(), ts));
        } else {
            terminal.push((id.clone(), ts));
        }
    }
    let mut remove: Vec<String> = terminal
        .iter()
        .filter(|(_, ts)| (now - *ts).num_days() > RETENTION_DAYS)
        .map(|(id, _)| id.clone())
        .collect();
    terminal.sort_by(|a, b| b.1.cmp(&a.1));
    if terminal.len() > MAX_TERMINAL_JOBS {
        remove.extend(terminal.iter().skip(MAX_TERMINAL_JOBS).map(|(id, _)| id.clone()));
    }
    active.sort_by(|a, b| b.1.cmp(&a.1));
    if active.len() > MAX_ACTIVE_JOBS {
        remove.extend(active.iter().skip(MAX_ACTIVE_JOBS).map(|(id, _)| id.clone()));
    }
    remove.sort();
    remove.dedup();
    for id in remove {
        map.remove(&id);
    }
}

/// 原子写入并收紧权限（Unix 下 0600）。先写临时文件再重命名，避免半截文件。
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelConfig;

    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mcspr-jobs-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = PanelConfig {
            data_dir: dir.to_string_lossy().to_string(),
            token: "test".into(),
            ..Default::default()
        };
        let state = AppState::new(cfg).await.unwrap();
        (state, dir)
    }

    #[tokio::test]
    async fn create_sets_metadata_and_persists() {
        let (state, dir) = test_state("meta").await;
        let id = create_job(
            &state,
            NewJob {
                kind: "modpack-import".into(),
                title: "导入整合包「demo」".into(),
                instance_id: None,
                user_id: Some("u1".into()),
                operation_id: Some("op-1".into()),
            },
        );
        let job = all(&state).into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(job.kind.as_deref(), Some("modpack-import"));
        assert_eq!(job.title.as_deref(), Some("导入整合包「demo」"));
        assert_eq!(job.user_id.as_deref(), Some("u1"));
        assert_eq!(job.status, "running");
        assert!(state.tasks_dir.join("jobs.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn running_jobs_become_interrupted_after_restart() {
        let (state, dir) = test_state("restart").await;
        let id = create_job(
            &state,
            NewJob {
                kind: "java-install".into(),
                title: "安装 Java 21".into(),
                ..Default::default()
            },
        );
        persist_jobs(&state, true);
        // 释放账户存储文件锁，模拟重启
        drop(state);
        // 模拟重启：新建状态并从磁盘恢复
        let cfg = PanelConfig {
            data_dir: dir.to_string_lossy().to_string(),
            token: "test".into(),
            ..Default::default()
        };
        let restarted = AppState::new(cfg).await.unwrap();
        let job = all(&restarted).into_iter().find(|j| j.id == id).unwrap();
        assert_eq!(job.status, "interrupted");
        assert_eq!(job.stage, "interrupted");
        assert!(job.finished_at.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn prune_keeps_bounded_history() {
        let (state, dir) = test_state("prune").await;
        for i in 0..(MAX_TERMINAL_JOBS + 20) {
            let id = create_job(
                &state,
                NewJob {
                    kind: "java-install".into(),
                    title: format!("job {i}"),
                    ..Default::default()
                },
            );
            finish_job(&state, &id, None, None);
        }
        let total = all(&state).len();
        assert!(total <= MAX_TERMINAL_JOBS, "terminal jobs not bounded: {total}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
