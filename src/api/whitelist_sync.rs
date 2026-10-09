//! 管理员白名单同步：状态 / 重试接口与后台批次协调（统一任务中心记录每实例结果）。

use crate::auth::Identity;
use crate::error::ApiResult;
use crate::instance::get_instance;
use crate::instance::whitelist_sync::{self, DesiredSnapshot, SyncOutcome, SyncStatus};
use crate::jobs::{create_job, finish_job, log_job, NewJob};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// 周期协调间隔（作为触发丢失 / 未覆盖路径的兜底）
const SYNC_INTERVAL: Duration = Duration::from_secs(300);
/// 批次 / 重试任务的统一类型
const JOB_KIND: &str = "whitelist-sync";

/// 触发信号：合并同一时间段内的多次请求，仅保留一次待处理协调。
static SYNC_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// 上次已协调的账户修订号（`u64::MAX` 表示尚未协调）
static LAST_REVISION: AtomicU64 = AtomicU64::new(u64::MAX);
/// 上次已协调的实例集合指纹（新实例 / 删除即使 revision 不变也需协调）
static LAST_INSTANCE_FP: AtomicU64 = AtomicU64::new(u64::MAX);
/// 强制协调：恢复 / 重装 / 身份覆盖变更即使 revision 与状态未变也必须重跑
static FORCE: AtomicBool = AtomicBool::new(true);

/// `GET /instances/{id}/whitelist-sync`：返回来源登记（`managed` / `desired` / 人工保留）、
/// 待重试成员与最近错误。管理员限定（见 `api/mod.rs::classify`）。
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    Ok(Json(whitelist_sync::status_view(&rt).await))
}

/// `POST /instances/{id}/whitelist-sync/retry`：按当前账户授权（权威快照）重新协调该实例。
///
/// 始终以当前账户修订号与已批准授权为准，并记录统一任务。
pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    let snapshot = auth_snapshot(&state).await;
    let desired = whitelist_sync::effective_members(&snapshot, &id);
    let name = rt.meta.read().await.name.clone();
    let job_id = create_job(
        &state,
        NewJob {
            kind: JOB_KIND.into(),
            title: format!("白名单同步重试「{name}」"),
            instance_id: Some(id.clone()),
            user_id: Some(identity.user_id.clone()),
            operation_id: None,
        },
    );
    log_job(
        &state,
        &job_id,
        format!("按当前授权（revision {}）重新协调该实例", snapshot.revision),
    );
    let outcome =
        whitelist_sync::reconcile_instance(&state, &rt, &desired, snapshot.revision).await;
    let ok = is_settled(&outcome);
    log_job(&state, &job_id, outcome_summary(&outcome));
    let err = if ok {
        None
    } else {
        Some(format!("白名单未完全生效：{}", status_label(&outcome)))
    };
    finish_job(&state, &job_id, err, Some(id.clone()));
    trigger_force();
    Ok(Json(
        json!({ "ok": ok, "job_id": job_id, "outcome": outcome }),
    ))
}

async fn reconcile_snapshot(
    state: &AppState,
    snapshot: &DesiredSnapshot,
) -> whitelist_sync::SyncAllReport {
    let report = whitelist_sync::sync_all(state, snapshot).await;
    if let Err(error) = crate::instance::name_reservations::release_confirmed(state, &report).await
    {
        tracing::warn!(%error, "旧游戏名移除确认失败，继续保留预留");
    }
    report
}

/// 请求一次后台协调（非阻塞，变化检测）。
pub fn trigger() {
    SYNC_NOTIFY.notify_one();
}

/// 请求一次强制后台协调（恢复 / 重装 / 身份覆盖变更等无法由 revision 体现的改动）。
pub fn trigger_force() {
    FORCE.store(true, Ordering::SeqCst);
    SYNC_NOTIFY.notify_one();
}

/// 启动后台白名单协调器：启动即协调一次，随后按周期兜底并响应 [`trigger`]。
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SYNC_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = SYNC_NOTIFY.notified() => {}
            }
            let forced = FORCE.swap(false, Ordering::SeqCst);
            run_batch(&state, forced).await;
        }
    });
}

/// 构造权威快照（修订号与授权在同一账户读锁内取值）。
async fn auth_snapshot(state: &AppState) -> DesiredSnapshot {
    let (revision, grants) = state.auth.approved_grants_snapshot().await;
    DesiredSnapshot::from_grants(revision, grants)
}

/// 变化检测：仅强制、修订号变化、实例集合变化或存在未收敛实例时执行。
fn should_run_batch(
    forced: bool,
    revision: u64,
    last_revision: u64,
    fp: u64,
    last_fp: u64,
    unsettled: bool,
) -> bool {
    forced || revision != last_revision || fp != last_fp || unsettled
}

/// 一次批次协调：变化检测通过后执行，并写入统一任务（每实例一行结果）。
async fn run_batch(state: &AppState, forced: bool) {
    let snapshot = auth_snapshot(state).await;
    let fp = instance_fingerprint(state).await;
    let last_revision = LAST_REVISION.load(Ordering::SeqCst);
    let last_fp = LAST_INSTANCE_FP.load(Ordering::SeqCst);
    let unchanged = !forced && snapshot.revision == last_revision && fp == last_fp;
    let unsettled = if unchanged {
        any_unsettled(state).await
    } else {
        false
    };
    if !should_run_batch(
        forced,
        snapshot.revision,
        last_revision,
        fp,
        last_fp,
        unsettled,
    ) {
        return;
    }
    LAST_REVISION.store(snapshot.revision, Ordering::SeqCst);
    LAST_INSTANCE_FP.store(fp, Ordering::SeqCst);

    let instance_count = state.instances.read().await.len();
    // 无实例且非强制：更新代号后不建任务，避免空批次刷历史
    if instance_count == 0 && !forced {
        return;
    }
    let job_id = create_job(
        state,
        NewJob {
            kind: JOB_KIND.into(),
            title: "白名单同步".into(),
            instance_id: None,
            user_id: None,
            operation_id: None,
        },
    );
    log_job(
        state,
        &job_id,
        format!(
            "开始协调 {instance_count} 个实例（revision {}）",
            snapshot.revision
        ),
    );
    let report = reconcile_snapshot(state, &snapshot).await;
    for o in &report.outcomes {
        log_job(state, &job_id, outcome_summary(o));
    }
    let unsettled_count = report.outcomes.iter().filter(|o| !is_settled(o)).count();
    let err = if unsettled_count == 0 {
        None
    } else {
        Some(format!(
            "{unsettled_count} 个实例未完全收敛（部分生效 / 待重试 / 错误），详见上方每实例结果"
        ))
    };
    finish_job(state, &job_id, err, None);
}

/// 是否存在未收敛实例（pending / partial / error / needs_override / applying）。
async fn any_unsettled(state: &AppState) -> bool {
    let instances: Vec<_> = state.instances.read().await.values().cloned().collect();
    for rt in instances {
        let view = whitelist_sync::status_view(&rt).await;
        if matches!(
            view.get("status").and_then(|s| s.as_str()),
            Some("pending" | "partial" | "error" | "needs_override" | "applying")
        ) {
            return true;
        }
    }
    false
}

/// 实例集合指纹：新实例 / 删除即使 revision 不变也需协调。
async fn instance_fingerprint(state: &AppState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut ids: Vec<String> = state.instances.read().await.keys().cloned().collect();
    ids.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ids.hash(&mut hasher);
    hasher.finish()
}

fn is_settled(o: &SyncOutcome) -> bool {
    matches!(
        o.status,
        SyncStatus::Applied | SyncStatus::Idle | SyncStatus::Skipped
    )
}

fn status_label(o: &SyncOutcome) -> &'static str {
    match o.status {
        SyncStatus::Idle => "无待同步成员",
        SyncStatus::Applied => "已生效",
        SyncStatus::Partial => "部分生效（仍有待重试成员）",
        SyncStatus::Pending => "待重试（命令超时或实例未就绪）",
        SyncStatus::Error => "错误（白名单或状态文件损坏 / IO 失败）",
        SyncStatus::Unsupported => "不支持（代理服务端无实例白名单）",
        SyncStatus::NeedsOverride => "需显式指定白名单身份模式",
        SyncStatus::Skipped => "已跳过（代号陈旧）",
    }
}

fn outcome_summary(o: &SyncOutcome) -> String {
    let mut line = format!("实例「{}」: {}", o.instance_id, status_label(o));
    if !o.added.is_empty() {
        line.push_str(&format!("；新增 {}", o.added.join("、")));
    }
    if !o.removed.is_empty() {
        line.push_str(&format!("；移除 {}", o.removed.join("、")));
    }
    if !o.pending.is_empty() {
        line.push_str(&format!("；待重试 {} 人", o.pending.len()));
    }
    if let Some(e) = &o.error {
        line.push_str(&format!("（{e}）"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelConfig;
    use crate::instance::{InstanceMeta, InstanceRuntime, Status};
    use std::sync::Mutex;

    /// 批次测试共享全局代号，串行执行避免互相干扰。
    static BATCH_TEST_LOCK: Mutex<()> = Mutex::new(());

    async fn setup(tag: &str) -> (AppState, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("mcspr-wlsync-api-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::new(PanelConfig {
            data_dir: root.join("data").to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
        state
            .auth
            .create_user("root", "password123", crate::auth::Role::Admin, vec![])
            .await
            .unwrap();
        (state, root)
    }

    async fn add_offline_instance(state: &AppState, id: &str) {
        let dir = state.config.read().await.instances_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("server.properties"), "online-mode=false\n").unwrap();
        let meta = InstanceMeta {
            id: id.to_string(),
            name: format!("inst-{id}"),
            ..Default::default()
        };
        std::fs::write(
            dir.join("instance.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();
        let rt = InstanceRuntime::new(meta, dir);
        state.instances.write().await.insert(id.to_string(), rt);
    }

    async fn grant_named(state: &AppState, username: &str, name: &str, instance: &str) -> String {
        let user = state
            .auth
            .register_pending_user(username, "password123", name, "r")
            .await
            .unwrap();
        state
            .auth
            .approve_application(&user.id, 1, vec![instance.to_string()], "root")
            .await
            .unwrap();
        user.id
    }

    fn reset_tracking() {
        LAST_REVISION.store(u64::MAX, Ordering::SeqCst);
        LAST_INSTANCE_FP.store(u64::MAX, Ordering::SeqCst);
        FORCE.store(false, Ordering::SeqCst);
    }

    fn whitelist_jobs(state: &AppState) -> Vec<crate::jobs::Job> {
        crate::jobs::all(state)
            .into_iter()
            .filter(|j| j.kind.as_deref() == Some(JOB_KIND))
            .collect()
    }

    #[test]
    fn should_run_batch_detects_changes() {
        assert!(should_run_batch(true, 1, 1, 1, 1, false), "强制必须执行");
        assert!(should_run_batch(false, 2, 1, 1, 1, false), "revision 变化");
        assert!(should_run_batch(false, 1, 1, 2, 1, false), "实例集合变化");
        assert!(should_run_batch(false, 1, 1, 1, 1, true), "存在未收敛实例");
        assert!(!should_run_batch(false, 1, 1, 1, 1, false), "无变化应跳过");
    }

    #[tokio::test]
    async fn batch_records_job_and_skips_unchanged() {
        let _g = BATCH_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (state, root) = setup("batch").await;
        add_offline_instance(&state, "i1").await;
        grant_named(&state, "bob", "Bob", "i1").await;
        reset_tracking();

        run_batch(&state, false).await;
        let first = whitelist_jobs(&state);
        assert_eq!(first.len(), 1, "首次批次应记录任务");
        assert_eq!(first[0].status, "done", "{:?}", first[0].error);
        assert!(
            first[0].user_id.is_none(),
            "自动批次任务无发起人（仅管理员可见）"
        );
        assert!(
            first[0].logs.iter().any(|l| l.contains("已生效")),
            "应记录每实例结果"
        );

        // 无变化：不重复建任务
        run_batch(&state, false).await;
        assert_eq!(whitelist_jobs(&state).len(), 1, "无变化不应产生历史任务");

        // 授权变化：revision 前进 → 新任务
        let bob = state.auth.find_by_username("bob").await.unwrap();
        state.auth.set_instances(&bob.id, vec![]).await.unwrap();
        run_batch(&state, false).await;
        assert_eq!(whitelist_jobs(&state).len(), 2, "授权变化应产生新任务");

        // 强制：即使无变化也执行
        run_batch(&state, true).await;
        assert_eq!(whitelist_jobs(&state).len(), 3, "强制协调应执行");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn batch_marks_partial_failure() {
        let _g = BATCH_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (state, root) = setup("partial").await;
        add_offline_instance(&state, "i1").await;
        grant_named(&state, "bob", "Bob", "i1").await;
        reset_tracking();
        // 实例运行中且无命令通道 → 命令失败，批次应明确标记未收敛
        {
            let rt = state.instances.read().await.get("i1").cloned().unwrap();
            *rt.status.lock().await = Status::Running;
        }
        run_batch(&state, false).await;
        let job = whitelist_jobs(&state).into_iter().next().unwrap();
        assert_eq!(job.status, "error", "未收敛批次应以 error 结束");
        assert!(job.error.as_deref().unwrap_or("").contains("未完全收敛"));
        assert!(
            job.logs.iter().any(|l| l.contains("待重试")),
            "应明确记录待重试原因"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
