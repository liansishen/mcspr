//! 实例白名单同步状态与重试接口。
//!
//! 路由由父模块在 `api/mod.rs` 接入（本模块不自行注册）：
//! - `GET  /instances/{id}/whitelist-sync`        → [`get`]
//! - `POST /instances/{id}/whitelist-sync/retry`  → [`retry`]
//!
//! ## 集成提案（父模块执行）
//!
//! 1. 在 `api/mod.rs` 声明 `mod whitelist_sync;` 并注册两条路由：
//!    ```ignore
//!    .route("/instances/{id}/whitelist-sync", get(whitelist_sync::get))
//!    .route("/instances/{id}/whitelist-sync/retry", post(whitelist_sync::retry))
//!    ```
//!    `classify()` 中两条路由归入 `Access::Admin`（未列入只读集合即默认管理员），
//!    或按需改为 `Access::Authenticated` 并携带实例 ID，便于普通用户查看自己的同步状态。
//! 2. 全局同步：在账户审批 / 授权 / 撤权 / 禁用 / 删除 / 游戏名变更落盘后，用
//!    `AuthStore::approved_named_grants()` 与 `AuthStore::revision()` 构造快照：
//!    ```ignore
//!    let grants = state.auth.approved_named_grants().await; // Vec<(user_id, name, Vec<instance_id>, is_admin)>
//!    let revision = state.auth.revision().await;
//!    let snapshot = DesiredSnapshot::from_grants(revision, grants);
//!    let report = whitelist_sync::sync_all(&state, &snapshot).await;
//!    ```
//!    建议放入后台任务，避免阻塞写请求；`report.outcomes` 暴露每个实例的状态与待重试成员。
//! 3. 周期性协调：面板启动后及每隔固定间隔（例如 5 分钟）重建快照并调用 `sync_all`，
//!    用于收敛运行中实例的命令超时、重启恢复与代理身份修正；同一实例由实例级锁串行。
//! 4. 生命周期钩子：在实例新建 / 启动 / 更新 / 重装 / 恢复流程中调用
//!    `whitelist_sync::reconcile_lifecycle(&state, &rt)`，返回 `None` 表示该实例从未同步。
//! 5. 手动白名单新增已在 `resources::users_action` 中调用
//!    `whitelist_sync::mark_manual_retained`，无需额外接线。
//!

use crate::error::ApiResult;
use crate::instance::whitelist_sync::{self, SyncStatus};
use crate::instance::get_instance;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

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
