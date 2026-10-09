//! 统一任务中心：任务元数据、有界持久化与崩溃恢复。
//!
//! 任务同时存在于内存映射（`AppState::jobs`）与 `data/tasks/jobs.json`。
//! 面板重启后把仍在 `running` 的任务标记为 `interrupted`，不做盲目重放。
//!
//! 持久化约定：
//! - 快照 + 落盘在 `AppState::persist_lock` 内串行执行，避免迟到旧快照覆盖新快照；
//! - 写入使用唯一临时文件（create_new + 0600）、fsync、rename、目录 fsync；
//! - 运行中的任务不会被裁剪，达到 `MAX_ACTIVE_JOBS` 时登记失败并返回明确错误。

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;

/// 单条任务保留的最大日志行数
pub const MAX_LOGS: usize = 400;
/// 终态任务保留上限
pub const MAX_TERMINAL_JOBS: usize = 1000;
/// 运行中任务上限（达到后拒绝新任务，而不是静默丢弃运行中任务）
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
///
/// 达到运行中任务上限或持久化失败时返回明确错误；失败时回滚内存登记。
pub fn create_job(state: &AppState, spec: NewJob) -> Result<String, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut job = Job::new(id.clone());
    job.kind = Some(spec.kind).filter(|k| !k.is_empty());
    job.title = Some(spec.title).filter(|t| !t.is_empty());
    job.instance_id = spec.instance_id;
    job.user_id = spec.user_id;
    job.operation_id = spec.operation_id;
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if map.values().filter(|j| j.status == "running").count() >= MAX_ACTIVE_JOBS {
            return Err(format!(
                "当前运行中的任务已达上限（{MAX_ACTIVE_JOBS}），请稍后再试"
            ));
        }
        map.insert(id.clone(), job);
    }
    if let Err(e) = persist_jobs(state, true) {
        state
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        return Err(format!("任务登记持久化失败: {e}"));
    }
    Ok(id)
}

/// 当前运行中的任务数量。
pub fn active_count(state: &AppState) -> usize {
    state
        .jobs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter(|j| j.status == "running")
        .count()
}

pub fn log_job(state: &AppState, id: &str, msg: impl Into<String>) {
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = map.get_mut(id) {
            j.log(msg);
        }
    }
    if let Err(e) = persist_jobs(state, false) {
        tracing::warn!("任务日志持久化失败: {e}");
    }
}

pub fn set_progress(state: &AppState, id: &str, pct: u8) {
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = map.get_mut(id) {
            j.progress = pct.min(100);
            j.updated_at = Some(now_iso());
        }
    }
    if let Err(e) = persist_jobs(state, false) {
        tracing::warn!("任务进度持久化失败: {e}");
    }
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
    if let Err(e) = persist_jobs(state, true) {
        tracing::warn!(%e, "任务完成状态持久化失败");
    }
    if instance_id.is_some() && kind.as_deref() != Some("whitelist-sync") {
        crate::api::whitelist_sync::trigger();
    }
}

/// 完成一个由操作幂等层登记的短任务（写入简短结果）。
pub fn finish_operation_job(
    state: &AppState,
    id: &str,
    ok: bool,
    result: Option<serde_json::Value>,
    error: Option<String>,
) {
    {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = map.get_mut(id) {
            j.status = if ok { "done".into() } else { "error".into() };
            j.stage = if ok { "done".into() } else { "error".into() };
            j.result = result;
            if let Some(e) = &error {
                j.error = Some(e.clone());
                j.log(format!("❌ {e}"));
            }
            let ts = now_iso();
            j.finished_at = Some(ts.clone());
            j.updated_at = Some(ts);
        }
    }
    if let Err(e) = persist_jobs(state, true) {
        tracing::warn!("操作任务持久化失败: {e}");
    }
}

/// 把后台任务与发起它的操作编号 / 账户关联起来（缺失时补全）。
pub fn attach_operation(state: &AppState, job_id: &str, operation_id: &str, user_id: &str) {
    let changed = {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        match map.get_mut(job_id) {
            Some(j) => {
                if j.operation_id.is_none() {
                    j.operation_id = Some(operation_id.to_string());
                }
                if j.user_id.is_none() {
                    j.user_id = Some(user_id.to_string());
                }
                true
            }
            None => false,
        }
    };
    if changed {
        if let Err(e) = persist_jobs(state, true) {
            tracing::warn!("后台任务关联持久化失败: {e}");
        }
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
///
/// 快照与写入在 `persist_lock` 内串行，保证不会出现迟到旧快照覆盖新快照。
pub fn persist_jobs(state: &AppState, force: bool) -> std::io::Result<()> {
    if !force {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = state.job_persist_at.load(Ordering::Relaxed);
        if now.saturating_sub(last) < PERSIST_THROTTLE_MS {
            return Ok(());
        }
        state.job_persist_at.store(now, Ordering::Relaxed);
    }
    let _guard = state.persist_lock.lock().unwrap_or_else(|p| p.into_inner());
    let json = {
        let mut map = state.jobs.lock().unwrap_or_else(|p| p.into_inner());
        prune_jobs(&mut map);
        serde_json::to_string(&*map)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
    };
    write_private(&state.tasks_dir.join("jobs.json"), json.as_bytes())
}

/// 面板启动时加载持久化任务；运行中任务标记为中断，不自动重放。
///
/// 文件损坏时保留原文件（改名为 `jobs.json.corrupt-<ts>`）并以空历史启动，
/// 避免默默覆盖证据。
pub fn restore(state: &AppState) -> Result<(), String> {
    let path = state.tasks_dir.join("jobs.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("读取任务历史失败: {e}")),
    };
    let map = match serde_json::from_str::<HashMap<String, Job>>(&text) {
        Ok(m) => m,
        Err(e) => {
            let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            let corrupt = state.tasks_dir.join(format!("jobs.json.corrupt-{ts}"));
            let _ = std::fs::rename(&path, &corrupt);
            tracing::error!(
                "任务历史损坏（{e}），原文件已保留为 {}，本次以空历史启动",
                corrupt.display()
            );
            return Ok(());
        }
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
    Ok(())
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

/// 只裁剪终态任务：过期的、以及超出上限的最旧终态记录。
/// 运行中的任务永不删除（登记上限由 `create_job` 控制）。
fn prune_jobs(map: &mut HashMap<String, Job>) {
    let now = chrono::Utc::now();
    let mut terminal: Vec<(String, chrono::DateTime<chrono::Utc>)> = map
        .iter()
        .filter(|(_, job)| job.status != "running")
        .map(|(id, job)| {
            let ts = chrono::DateTime::parse_from_rfc3339(&job.created_at)
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or(now);
            (id.clone(), ts)
        })
        .collect();
    let mut remove: Vec<String> = terminal
        .iter()
        .filter(|(_, ts)| (now - *ts).num_days() > RETENTION_DAYS)
        .map(|(id, _)| id.clone())
        .collect();
    terminal.sort_by(|a, b| b.1.cmp(&a.1));
    if terminal.len() > MAX_TERMINAL_JOBS {
        remove.extend(
            terminal
                .iter()
                .skip(MAX_TERMINAL_JOBS)
                .map(|(id, _)| id.clone()),
        );
    }
    remove.sort();
    remove.dedup();
    for id in remove {
        map.remove(&id);
    }
}

/// 原子写入并收紧权限（Unix 下 0600）。先写唯一临时文件再重命名，
/// fsync 文件与目录；拒绝写入符号链接目标或位于符号链接目录下的路径。
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::ErrorKind;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "路径缺少父目录"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    if let Ok(md) = std::fs::symlink_metadata(parent) {
        if md.file_type().is_symlink() {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "拒绝写入符号链接目录",
            ));
        }
    }
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.file_type().is_symlink() {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "拒绝覆盖符号链接目标",
            ));
        }
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "data".to_string());
    let tmp = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    let write_res = (|| -> std::io::Result<()> {
        file.write_all(data)?;
        file.sync_all()
    })();
    if let Err(e) = write_res {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    drop(file);
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelConfig;

    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("mcspr-jobs-test-{tag}-{}", uuid::Uuid::new_v4()));
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
        )
        .unwrap();
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
        )
        .unwrap();
        persist_jobs(&state, true).unwrap();
        // 释放账户存储文件锁，模拟重启
        drop(state);
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
            )
            .unwrap();
            finish_job(&state, &id, None, None);
        }
        let total = all(&state).len();
        assert!(
            total <= MAX_TERMINAL_JOBS,
            "terminal jobs not bounded: {total}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn active_jobs_are_never_pruned() {
        let (state, dir) = test_state("active").await;
        // 直接注入超过上限的运行中任务，验证 prune 不删除它们
        {
            let mut map = state.jobs.lock().unwrap();
            for i in 0..(MAX_ACTIVE_JOBS + 40) {
                let mut job = Job::new(format!("active-{i}"));
                job.status = "running".into();
                map.insert(job.id.clone(), job);
            }
        }
        persist_jobs(&state, true).unwrap();
        assert_eq!(
            all(&state).len(),
            MAX_ACTIVE_JOBS + 40,
            "运行中任务不应被裁剪"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn admission_rejects_when_active_full() {
        let (state, dir) = test_state("admission").await;
        {
            let mut map = state.jobs.lock().unwrap();
            for i in 0..MAX_ACTIVE_JOBS {
                map.insert(format!("a{i}"), Job::new(format!("a{i}")));
            }
        }
        let err = create_job(
            &state,
            NewJob {
                kind: "java-install".into(),
                title: "x".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("上限"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn concurrent_admission_keeps_active_limit() {
        let (state, dir) = test_state("admission-race").await;
        {
            let mut map = state.jobs.lock().unwrap();
            for i in 0..MAX_ACTIVE_JOBS - 1 {
                map.insert(format!("a{i}"), Job::new(format!("a{i}")));
            }
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    create_job(
                        &state,
                        NewJob {
                            kind: "test".into(),
                            ..Default::default()
                        },
                    )
                    .is_ok()
                })
            })
            .collect();
        let admitted = handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, 1);
        assert_eq!(active_count(&state), MAX_ACTIVE_JOBS);
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_persist_never_corrupts_file() {
        let (state, dir) = test_state("race").await;
        let mut handles = Vec::new();
        for i in 0..32 {
            let st = state.clone();
            handles.push(tokio::spawn(async move {
                let _ = create_job(
                    &st,
                    NewJob {
                        kind: "java-install".into(),
                        title: format!("race {i}"),
                        ..Default::default()
                    },
                );
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        persist_jobs(&state, true).unwrap();
        let text = std::fs::read_to_string(state.tasks_dir.join("jobs.json")).unwrap();
        let parsed: HashMap<String, Job> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.len(), 32, "并发登记后应完整可解析");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn corrupt_history_is_preserved_and_startup_continues() {
        let (state, dir) = test_state("corrupt").await;
        persist_jobs(&state, true).unwrap();
        drop(state);
        std::fs::write(dir.join("tasks/jobs.json"), b"{ this is not json").unwrap();
        let cfg = PanelConfig {
            data_dir: dir.to_string_lossy().to_string(),
            token: "test".into(),
            ..Default::default()
        };
        let restarted = AppState::new(cfg).await.unwrap();
        assert!(all(&restarted).is_empty());
        let preserved: Vec<_> = std::fs::read_dir(dir.join("tasks"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(preserved.len(), 1, "损坏文件应被保留");
        let _ = std::fs::remove_dir_all(dir);
    }
}
