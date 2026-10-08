use crate::auth::Identity;
use crate::error::ApiResult;
use crate::instance::{get_instance, InstanceRuntime, LogLine};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::{Extension, Json};
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
    let cursor = buf.back().map(|l| l.seq).unwrap_or(0);
    Ok(Json(json!({ "cursor": cursor, "lines": lines })))
}

pub async fn ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<Identity>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let rt = get_instance(&state, &id).await?;
    Ok(ws.on_upgrade(move |socket| ws_task(socket, rt, state, identity)))
}

async fn ws_task(
    socket: WebSocket,
    rt: Arc<InstanceRuntime>,
    state: AppState,
    identity: Identity,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = rt.log_tx.subscribe();
    let history: Vec<_> = rt.log_buf.lock().await.iter().cloned().collect();
    let mut last_sent = history.last().map(|l| l.seq).unwrap_or(0);
    if sender.send(history_msg(&history, last_sent)).await.is_err() {
        return;
    }
    // 周期校验会话（1s）：撤销 / 到期 / 角色变化后快速关闭连接
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if !still_admin(&state, &identity).await {
                    let _ = sender.send(Message::Close(None)).await;
                    break;
                }
            }
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let buf = rt.log_buf.lock().await;
                        let missed = buffered_after(&buf, last_sent);
                        for line in missed {
                            if sender.send(text_msg(&line)).await.is_err() { return; }
                            last_sent = line.seq;
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        // 发送命令前重新校验当前权限
                        if !still_admin(&state, &identity).await {
                            let _ = sender.send(Message::Close(None)).await;
                            break;
                        }
                        let _ = crate::instance::process::send_command(&rt, t.trim()).await;
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }
}

/// 会话撤销 / 到期 / 角色变化后判定连接不再具备管理员权限。
async fn still_admin(state: &AppState, identity: &Identity) -> bool {
    match state.auth.session_identity_by_digest(&identity.session).await {
        Some(current) => current.is_admin(),
        None => false,
    }
}

fn text_msg(l: &LogLine) -> Message {
    Message::Text(serde_json::to_string(l).unwrap_or_default().into())
}

fn history_msg(lines: &[LogLine], cursor: u64) -> Message {
    Message::Text(
        json!({ "type": "history", "lines": lines, "cursor": cursor })
            .to_string()
            .into(),
    )
}

fn buffered_after(lines: &std::collections::VecDeque<LogLine>, last_sent: u64) -> Vec<LogLine> {
    lines
        .iter()
        .filter(|line| line.seq > last_sent)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_one_batch_and_live_lines_keep_their_shape() {
        let line = LogLine {
            seq: 7,
            ts: "12:00:00".into(),
            line: "<tag> INFO".into(),
        };
        let Message::Text(history) = history_msg(&[line.clone()], 7) else {
            panic!()
        };
        let value: serde_json::Value = serde_json::from_str(&history).unwrap();
        assert_eq!(value["type"], "history");
        assert_eq!(value["cursor"], 7);
        assert_eq!(value["lines"][0]["line"], "<tag> INFO");
        let Message::Text(live) = text_msg(&line) else {
            panic!()
        };
        let value: serde_json::Value = serde_json::from_str(&live).unwrap();
        assert_eq!(value["seq"], 7);
        assert!(value.get("type").is_none());
    }

    #[test]
    fn buffered_replay_skips_already_sent_lines_and_includes_missed_lines() {
        let lines = std::collections::VecDeque::from(vec![
            LogLine {
                seq: 1,
                ts: "t1".into(),
                line: "one".into(),
            },
            LogLine {
                seq: 2,
                ts: "t2".into(),
                line: "two".into(),
            },
            LogLine {
                seq: 3,
                ts: "t3".into(),
                line: "three".into(),
            },
        ]);
        let replay = buffered_after(&lines, 1);
        assert_eq!(
            replay.iter().map(|line| line.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(buffered_after(&lines, 3).is_empty());
    }

    #[test]
    fn empty_console_buffer_resets_cursor_to_zero() {
        let lines: std::collections::VecDeque<LogLine> = std::collections::VecDeque::new();
        let cursor = lines.back().map(|line| line.seq).unwrap_or(0);
        assert_eq!(cursor, 0);
    }
}
