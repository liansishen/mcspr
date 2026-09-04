//! 实例计划任务：定时执行命令 / 备份 / 重启

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    /// command | backup | restart
    pub kind: String,
    /// command 类的命令内容
    pub value: String,
    /// 间隔（分钟）
    pub interval_mins: u64,
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run: String,
    #[serde(default)]
    pub last_result: String,
    #[serde(default)]
    pub consecutive_failures: u32,
}

fn bool_true() -> bool {
    true
}

pub fn load(dir: &Path) -> Vec<Task> {
    std::fs::read_to_string(dir.join("tasks.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, tasks: &[Task]) -> std::io::Result<()> {
    std::fs::write(dir.join("tasks.json"), serde_json::to_string_pretty(tasks)?)
}

/// 执行单个任务（run_due 与手动“立即运行”共用）
async fn execute(
    state: &AppState,
    rt: &std::sync::Arc<crate::instance::InstanceRuntime>,
    t: &Task,
) -> (Option<bool>, String) {
    let running = *rt.status.lock().await == crate::instance::Status::Running;
    match t.kind.as_str() {
        "command" => {
            if !running {
                (None, "跳过：服务器未运行".to_string())
            } else {
                match crate::instance::process::send_command(rt, &t.value).await {
                    Ok(()) => (Some(true), format!("已执行: {}", t.value)),
                    Err(e) => (Some(false), format!("执行失败: {e}")),
                }
            }
        }
        "backup" => match crate::instance::backup::create(state, rt).await {
            Ok(name) => (Some(true), format!("备份完成: {name}")),
            Err(e) => (Some(false), format!("备份失败: {e}")),
        },
        "restart" => {
            if !running {
                (None, "跳过：服务器未运行".to_string())
            } else {
                let _ = crate::instance::process::stop(state.clone(), rt.clone()).await;
                match crate::instance::process::start(state.clone(), rt.clone()).await {
                    Ok(()) => (Some(true), "已重启".to_string()),
                    Err(e) => (Some(false), format!("重启失败: {e}")),
                }
            }
        }
        _ => (Some(false), format!("未知任务类型: {}", t.kind)),
    }
}

/// 到期检查与执行（由面板调度器每 30 秒调用一次）
pub async fn run_due(state: &AppState, rt: &std::sync::Arc<crate::instance::InstanceRuntime>) {
    let dir = rt.dir.clone();
    let mut tasks = load(&dir);
    let now = chrono::Local::now();
    let mut changed = false;
    for t in tasks.iter_mut() {
        if !t.enabled {
            continue;
        }
        // 用 last_run 解析到期时间；无 last_run 视为已到期
        let due = t
            .last_run
            .parse::<chrono::DateTime<chrono::Local>>()
            .map(|last| (now - last).num_minutes() >= t.interval_mins as i64)
            .unwrap_or(true);
        if !due {
            continue;
        }
        t.last_run = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let (ok, result) = execute(state, rt, t).await;
        match ok {
            Some(true) => {
                t.last_result = result;
                t.consecutive_failures = 0;
            }
            Some(false) => {
                t.last_result = result.clone();
                t.consecutive_failures += 1;
            }
            None => {
                t.last_result = result.clone();
                // 跳过不计入失败
            }
        }
        if t.consecutive_failures >= 3 {
            crate::alerts::send(
                state,
                "task_fail",
                format!(
                    "实例「{}」计划任务「{}」连续失败 {} 次：{}",
                    rt.meta.read().await.name, t.name, t.consecutive_failures, t.last_result
                ),
            )
            .await;
        }
        changed = true;
    }
    if changed {
        let _ = save(&dir, &tasks);
    }
}

/// 手动“立即运行”
pub async fn run_task_now(
    state: &AppState,
    rt: &std::sync::Arc<crate::instance::InstanceRuntime>,
    task_id: &str,
) {
    let tasks = load(&rt.dir);
    let Some(t) = tasks.iter().find(|t| t.id == task_id).cloned() else { return };
    let (ok, result) = execute(state, rt, &t).await;
    let mut tasks = load(&rt.dir);
    if let Some(task) = tasks.iter_mut().find(|x| x.id == task_id) {
        task.last_run = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        match ok {
            Some(true) => {
                task.last_result = result;
                task.consecutive_failures = 0;
            }
            Some(false) => {
                task.last_result = result.clone();
                task.consecutive_failures += 1;
            }
            None => task.last_result = result,
        }
        let should_alert = ok == Some(false) && task.consecutive_failures >= 3;
        let alert_msg = format!(
            "计划任务「{}」执行失败：{}",
            task.name, task.last_result
        );
        let alert_key = format!("task-{}", task.id);
        let _ = save(&rt.dir, &tasks);
        if should_alert {
            crate::alerts::send(state, &alert_key, alert_msg).await;
        }
    }
}
