//! 管理员白名单同步状态、重试与后台协调。

use crate::error::ApiResult;
use crate::instance::whitelist_sync::{self, DesiredSnapshot, SyncStatus};
use crate::instance::get_instance;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};
use std::time::Duration;

/// `GET /instances/{id}/whitelist-sync`：返回来源登记、待重试成员与最近错误。
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    Ok(Json(whitelist_sync::status_view(&rt).await))
}

/// `POST /instances/{id}/whitelist-sync/retry`：用已持久化的期望集合重新协调。
pub async fn retry(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    let outcome = whitelist_sync::retry_instance(&state, &rt).await;
    let ok = matches!(outcome.status, SyncStatus::Applied | SyncStatus::Idle);
    Ok(Json(json!({ "ok": ok, "outcome": outcome })))
}

/// 周期协调间隔（作为触发丢失 / 未覆盖路径的兜底）
const SYNC_INTERVAL: Duration = Duration::from_secs(300);

/// 触发信号：合并同一时间段内的多次请求，仅保留一次待处理协调。
static SYNC_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// 用当前认证存储构造一致快照并协调全部实例。
///
/// 修订号与授权在同一账户读锁内取值，避免 grants 与 revision 跨版本。
pub async fn reconcile_all(state: &AppState) -> whitelist_sync::SyncAllReport {
    let (revision, grants) = state.auth.approved_grants_snapshot().await;
    let snapshot = DesiredSnapshot::from_grants(revision, grants);
    let report = whitelist_sync::sync_all(state, &snapshot).await;
    if let Err(error) = crate::instance::name_reservations::release_confirmed(state, &report).await {
        tracing::warn!(%error, "旧游戏名移除确认失败，继续保留预留");
    }
    report
}

/// 请求一次后台协调（非阻塞）：实际协调由 [`spawn_scheduler`] 中的任务串行执行。
pub fn trigger() {
    SYNC_NOTIFY.notify_one();
}

/// 启动后台白名单协调器：启动时立即协调一次，随后按周期兜底并响应 [`trigger`]。
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SYNC_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = SYNC_NOTIFY.notified() => {}
            }
            let report = reconcile_all(&state).await;
            if !report.ok {
                tracing::debug!(revision = report.revision, "白名单协调未完全收敛，等待后续重试");
            }
        }
    });
}
