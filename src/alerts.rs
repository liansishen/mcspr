//! 告警推送：Webhook / Discord / Telegram，同类事件 5 分钟去重

use crate::state::AppState;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 发送告警（key 用于 5 分钟去重）；推送失败只记日志，绝不影响实例
pub async fn send(state: &AppState, key: &str, text: impl Into<String>) {
    let text = text.into();
    let (url, kind, tg_token, tg_chat) = {
        let c = state.config.read().await;
        (
            c.alert_webhook_url.clone(),
            c.alert_type.clone(),
            c.telegram_bot_token.clone(),
            c.telegram_chat_id.clone(),
        )
    };
    if url.trim().is_empty() || kind == "none" {
        return;
    }
    // 5 分钟去重
    {
        let mut dedup = state.alert_dedup.lock().unwrap();
        if let Some(t) = dedup.get(key) {
            if t.elapsed() < Duration::from_secs(300) {
                return;
            }
        }
        dedup.insert(key.to_string(), Instant::now());
    }
    let text = format!("[MCS Panel] {text}");
    tracing::warn!("告警: {text}");
    let http = state.http.clone();
    tokio::spawn(async move {
        let r = match kind.as_str() {
            "discord" => http.post(&url).json(&serde_json::json!({ "content": text })).send().await,
            "telegram" => {
                http.post(format!("https://api.telegram.org/bot{tg_token}/sendMessage"))
                    .json(&serde_json::json!({ "chat_id": tg_chat, "text": text }))
                    .send()
                    .await
            }
            _ => http.post(&url).json(&serde_json::json!({ "text": text })).send().await,
        };
        if let Err(e) = r {
            tracing::warn!("告警推送失败: {e}");
        }
    });
}
