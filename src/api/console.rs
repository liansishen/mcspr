use crate::error::ApiResult;
use crate::instance::{get_instance, InstanceRuntime, LogLine};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn console(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let rt = get_instance(&state, &id).await?;
    let after: u64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    let buf = rt.log_buf.lock().await;
    let lines: Vec<LogLine> = buf.iter().filter(|l| l.seq > after).cloned().collect();
    let cursor = buf.back().map(|l| l.seq).unwrap_or(after);
    Ok(Json(json!({ "cursor": cursor, "lines": lines })))
}

pub async fn ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let rt = get_instance(&state, &id).await?;
    Ok(ws.on_upgrade(move |socket| ws_task(socket, rt)))
}

async fn ws_task(socket: WebSocket, rt: Arc<InstanceRuntime>) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = rt.log_tx.subscribe();
    let mut last_sent: u64 = 0;

    // 先发历史日志
    {
        let buf = rt.log_buf.lock().await;
        for l in buf.iter() {
            last_sent = l.seq;
            if sender.send(text_msg(l)).await.is_err() {
                return;
            }
        }
    }

    loop {
        tokio::select! {
            line = rx.recv() => {
                match line {
                    Ok(l) => {
                        if l.seq <= last_sent {
                            continue;
                        }
                        last_sent = l.seq;
                        if sender.send(text_msg(&l)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        let _ = crate::instance::process::send_command(&rt, t.trim()).await;
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }
}

fn text_msg(l: &LogLine) -> Message {
    Message::Text(serde_json::to_string(l).unwrap_or_default().into())
}
