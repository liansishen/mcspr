//! 操作审计：写操作与失败请求记录（内存环形 + JSONL 落盘）

use crate::state::AppState;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
}

pub const CAP: usize = 5000;

pub async fn record(state: &AppState, method: &str, path: &str, status: u16) {
    let path = redact(path);
    let entry = AuditEntry {
        ts: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        method: method.to_string(),
        path,
        status,
    };
    let line = serde_json::to_string(&entry).unwrap_or_default();
    {
        let mut ring = state.audit.lock().await;
        ring.push_back(entry);
        while ring.len() > CAP {
            ring.pop_front();
        }
    }
    // 落盘
    let dir = Path::new(&state.config.read().await.data_dir).to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("audit.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// 脱敏：抹掉查询串中的 token / password / key
fn redact(path: &str) -> String {
    if !path.contains('?') {
        return path.to_string();
    }
    let (base, query) = path.split_once('?').unwrap();
    let cleaned: Vec<String> = query
        .split('&')
        .map(|pair| {
            let k = pair.split('=').next().unwrap_or("");
            if k.contains("token") || k.contains("password") || k.contains("key") {
                format!("{k}=[REDACTED]")
            } else {
                pair.to_string()
            }
        })
        .collect();
    format!("{base}?{}", cleaned.join("&"))
}

pub async fn query(state: &AppState, limit: usize, q: &str) -> Vec<AuditEntry> {
    let ring = state.audit.lock().await;
    ring.iter()
        .rev()
        .filter(|e| {
            q.is_empty()
                || e.path.to_lowercase().contains(&q.to_lowercase())
                || e.method.to_lowercase().contains(&q.to_lowercase())
                || e.status.to_string().contains(q)
        })
        .take(limit)
        .cloned()
        .collect()
}
