//! 实例运行信息只读快照：状态、TPS 与玩家实时时长

use crate::error::ApiResult;
use crate::instance::{get_instance, playtime_snapshot, tps_view, uptime};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

/// 运行信息快照（授权用户）。TPS 错误使用面向用户的安全提示。
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    let status = rt.display_status().await.to_string();
    let uptime_secs = uptime(&rt).await;
    let players = rt.players.lock().await.len();
    let tps = {
        let cached = rt.tps.lock().await.clone();
        tps_view(&status, cached.as_ref(), true)
    };
    let player_stats = playtime_snapshot(&rt).await;
    let meta = rt.meta.read().await;
    Ok(Json(json!({
        "id": meta.id,
        "name": meta.name,
        "status": status,
        "uptime_secs": uptime_secs,
        "players": players,
        "tps": tps,
        "player_stats": player_stats,
    })))
}
