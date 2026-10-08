//! 操作审计：写操作与失败请求记录（内存环形 + JSONL 落盘）

use crate::state::AppState;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    /// 操作者账户 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 操作者用户名（已登录请求；登录失败时为尝试的用户名）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 操作者角色（admin / user）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// 目标元数据（如 instances:{id} / accounts:{id}）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// 审计操作者信息。已登录请求由中间件写入响应扩展；登录接口可单独记录尝试的用户名。
#[derive(Debug, Clone, Default)]
pub struct AuditActor {
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub role: Option<String>,
}

impl AuditActor {
    pub fn from_identity(identity: &crate::auth::Identity) -> Self {
        Self {
            user_id: Some(identity.user_id.clone()),
            username: Some(identity.username.clone()),
            role: Some(identity.role.as_str().to_string()),
        }
    }
}

pub const CAP: usize = 5000;

pub async fn record(
    state: &AppState,
    method: &str,
    path: &str,
    status: u16,
    actor: Option<&AuditActor>,
    target: Option<&str>,
) {
    let path = redact(path);
    let entry = AuditEntry {
        ts: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        method: method.to_string(),
        path,
        status,
        user_id: actor.and_then(|a| a.user_id.clone()),
        user: actor.and_then(|a| a.username.clone()),
        role: actor.and_then(|a| a.role.clone()),
        target: target.map(|t| t.to_string()),
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
            let needle = q.to_lowercase();
            q.is_empty()
                || e.path.to_lowercase().contains(&needle)
                || e.method.to_lowercase().contains(&needle)
                || e.status.to_string().contains(q)
                || e.user_id
                    .as_deref()
                    .map(|u| u.to_lowercase().contains(&needle))
                    .unwrap_or(false)
                || e.user
                    .as_deref()
                    .map(|u| u.to_lowercase().contains(&needle))
                    .unwrap_or(false)
                || e.target
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(&needle))
                    .unwrap_or(false)
        })
        .take(limit)
        .cloned()
        .collect()
}
