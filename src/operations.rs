//! 写操作幂等登记：稳定 `X-Operation-ID`、签名指纹与结果重放。
//!
//! 客户端为每次写操作生成一个稳定编号；后端以 `(操作编号)` 为键，绑定
//! `(发起人, 方法, 路由, 内容指纹)`。同编号同内容返回原结果，同编号不同
//! 内容返回冲突，运行中重复请求返回“处理中”。所有记录有界持久化，面板
//! 重启后未完成的操作标记为 `interrupted`，不做盲目重放。

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 保留的操作记录上限
pub const MAX_OPERATIONS: usize = 2000;
/// 操作记录保留小时数
pub const RETENTION_HOURS: i64 = 24;
/// 操作编号长度上限
pub const MAX_OP_ID_LEN: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub principal: String,
    pub method: String,
    pub route: String,
    pub fingerprint: String,
    /// pending | done | error | interrupted
    pub status: String,
    pub status_code: u16,
    pub job_id: Option<String>,
    pub response: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    /// 终态结果是否已可靠落盘（落盘失败时为 false，重启后只能视为中断）
    #[serde(default = "default_true")]
    pub durable: bool,
}

/// 操作登记结果。
#[derive(Debug)]
pub enum Reserve {
    /// 首次登记，调用方可以执行副作用。
    Reserved,
    /// 已存在相同编号与内容：返回原结果 / 处理中状态。
    Replay(Operation),
    /// 相同编号但内容不同。
    Conflict,
    /// 相同编号但属于其他账户。
    PrincipalConflict,
}

/// 登记失败原因。
#[derive(Debug)]
pub enum ReserveError {
    /// 持久化失败：调用方必须拒绝执行副作用。
    Storage(String),
    /// 登记容量已满。
    Capacity,
}

pub struct OperationStore {
    pub map: std::sync::Mutex<HashMap<String, Operation>>,
    key: Vec<u8>,
    dir: PathBuf,
}

impl OperationStore {
    pub fn load(data_dir: &str) -> Self {
        let dir = Path::new(data_dir).join("tasks");
        let _ = std::fs::create_dir_all(&dir);
        let key = load_or_create_key(&dir);
        Self {
            map: std::sync::Mutex::new(HashMap::new()),
            key,
            dir,
        }
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn default_true() -> bool {
    true
}

/// 操作编号仅允许 URL 安全字符，避免日志与持久化注入。
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_OP_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// 计算签名指纹：HMAC-SHA256(secret, principal|method|route|query|body-digest)。
pub fn fingerprint(
    state: &AppState,
    principal: &str,
    method: &str,
    route: &str,
    query: &str,
    body: Option<&[u8]>,
) -> String {
    let digest = body.map(sha256_hex);
    fingerprint_digest(state, principal, method, route, query, digest.as_deref())
}

/// 以已计算的正文摘要（原始字节或多部分逻辑指纹）参与签名。
pub fn fingerprint_digest(
    state: &AppState,
    principal: &str,
    method: &str,
    route: &str,
    query: &str,
    body_digest: Option<&str>,
) -> String {
    let mut canonical = String::new();
    canonical.push_str(principal);
    canonical.push('\n');
    canonical.push_str(method);
    canonical.push('\n');
    canonical.push_str(route);
    canonical.push('\n');
    canonical.push_str(query);
    canonical.push('\n');
    canonical.push_str(body_digest.unwrap_or("no-body"));
    hex(&hmac_sha256(&state.operations.key, canonical.as_bytes()))
}

/// 正文 SHA-256 十六进制摘要。
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn reserve(
    state: &AppState,
    id: &str,
    principal: &str,
    method: &str,
    route: &str,
    fingerprint: &str,
) -> Result<Reserve, ReserveError> {
    {
        let mut map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(op) = map.get(id) {
            if op.principal != principal {
                return Ok(Reserve::PrincipalConflict);
            }
            if op.fingerprint != fingerprint {
                return Ok(Reserve::Conflict);
            }
            return Ok(Reserve::Replay(op.clone()));
        }
        // 容量：只裁剪终态记录，pending 绝不驱逐
        if map.len() >= MAX_OPERATIONS {
            prune(&mut map);
        }
        if map.len() >= MAX_OPERATIONS {
            return Err(ReserveError::Capacity);
        }
        let ts = now_iso();
        map.insert(
            id.to_string(),
            Operation {
                id: id.to_string(),
                principal: principal.to_string(),
                method: method.to_string(),
                route: route.to_string(),
                fingerprint: fingerprint.to_string(),
                status: "pending".into(),
                status_code: 0,
                job_id: None,
                response: None,
                created_at: ts.clone(),
                updated_at: ts,
                finished_at: None,
                durable: false,
            },
        );
    }
    // 必须先持久化成功才允许执行副作用；失败则回滚内存登记并拒绝请求
    if let Err(e) = persist(state) {
        state
            .operations
            .map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        return Err(ReserveError::Storage(e));
    }
    Ok(Reserve::Reserved)
}

pub fn complete(
    state: &AppState,
    id: &str,
    status_code: u16,
    response: Option<serde_json::Value>,
    job_id: Option<String>,
) {
    {
        let mut map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(op) = map.get_mut(id) {
            op.status = if status_code >= 400 { "error" } else { "done" }.into();
            op.status_code = status_code;
            op.response = response;
            op.job_id = job_id;
            op.durable = true;
            let ts = now_iso();
            op.updated_at = ts.clone();
            op.finished_at = Some(ts);
        }
    }
    if let Err(e) = persist(state) {
        // 不虚报可靠存储：标记未落盘，重启后该编号只会被视为中断
        let mut map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(op) = map.get_mut(id) {
            op.durable = false;
        }
        tracing::error!("操作 {id} 结果落盘失败: {e}；重启后将仅标记为中断，不做重放");
    }
}

/// 查询操作；普通账户仅能查询自己的操作。
pub fn lookup(state: &AppState, id: &str, principal: &str, admin: bool) -> Option<Operation> {
    let map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
    map.get(id)
        .filter(|op| admin || op.principal == principal)
        .cloned()
}

/// 按任务编号查找已登记的操作（用于失败任务重试时解除旧编号）。
pub fn find_by_job(state: &AppState, job_id: &str) -> Option<Operation> {
    let map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
    map.values()
        .find(|op| op.job_id.as_deref() == Some(job_id))
        .cloned()
}

/// 删除一条操作登记（显式重试时解除旧编号的幂等绑定）。
pub fn forget(state: &AppState, id: &str) -> bool {
    let removed = {
        let mut map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(id).is_some()
    };
    if removed {
        if let Err(e) = persist(state) {
            tracing::warn!("解除操作登记持久化失败: {e}");
        }
    }
    removed
}

/// 面板启动时恢复操作记录；未完成的操作标记为中断。
///
/// 账本损坏时拒绝启动（fail-closed），避免默默清空幂等记录。
pub fn restore(state: &AppState) -> Result<(), String> {
    let path = state.operations.dir.join("operations.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("读取操作账本失败: {e}")),
    };
    let saved = serde_json::from_str::<HashMap<String, Operation>>(&text)
        .map_err(|e| format!("操作账本损坏，拒绝启动以免丢失幂等记录: {e}"))?;
    let ts = now_iso();
    let mut map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
    for (id, mut op) in saved {
        if op.status == "pending" {
            op.status = "interrupted".into();
            op.updated_at = ts.clone();
            op.finished_at = Some(ts.clone());
            op.durable = false;
        }
        map.insert(id, op);
    }
    prune(&mut map);
    Ok(())
}

/// 快照 + 写入在 `persist_lock` 内串行，避免迟到旧快照覆盖新快照。
fn persist(state: &AppState) -> Result<(), String> {
    let _guard = state
        .persist_lock
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let json = {
        let mut map = state.operations.map.lock().unwrap_or_else(|p| p.into_inner());
        prune(&mut map);
        serde_json::to_string(&*map).map_err(|e| e.to_string())?
    };
    crate::jobs::write_private(&state.operations.dir.join("operations.json"), json.as_bytes())
        .map_err(|e| e.to_string())
}

/// 只裁剪终态记录：过期的、以及超出上限的最旧终态记录；pending 永不驱逐。
fn prune(map: &mut HashMap<String, Operation>) {
    let now = chrono::Utc::now();
    let mut remove: Vec<String> = map
        .iter()
        .filter(|(_, op)| op.status != "pending")
        .filter(|(_, op)| {
            chrono::DateTime::parse_from_rfc3339(&op.updated_at)
                .map(|d| (now - d.with_timezone(&chrono::Utc)).num_hours() > RETENTION_HOURS)
                .unwrap_or(false)
        })
        .map(|(id, _)| id.clone())
        .collect();
    let mut terminal: Vec<(String, String)> = map
        .iter()
        .filter(|(id, op)| op.status != "pending" && !remove.contains(id))
        .map(|(id, op)| (id.clone(), op.updated_at.clone()))
        .collect();
    if terminal.len() > MAX_OPERATIONS {
        terminal.sort_by(|a, b| a.1.cmp(&b.1));
        let excess = terminal.len() - MAX_OPERATIONS;
        remove.extend(terminal.into_iter().take(excess).map(|(id, _)| id));
    }
    remove.sort();
    remove.dedup();
    for id in remove {
        map.remove(&id);
    }
}

fn load_or_create_key(dir: &Path) -> Vec<u8> {
    let path = dir.join("operation.key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(bytes) = hex_decode(text.trim()) {
            if bytes.len() >= 16 {
                return bytes;
            }
        }
    }
    use rand::RngCore;
    let mut key = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    let _ = crate::jobs::write_private(&path, hex(&key).as_bytes());
    key
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    k.resize(BLOCK, 0);
    let mut ipad = vec![0x36u8; BLOCK];
    let mut opad = vec![0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelConfig;

    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mcspr-ops-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = PanelConfig {
            data_dir: dir.to_string_lossy().to_string(),
            token: "test".into(),
            ..Default::default()
        };
        let state = AppState::new(cfg).await.unwrap();
        (state, dir)
    }

    #[tokio::test]
    async fn reserve_replays_same_content_and_conflicts_on_change() {
        let (state, dir) = test_state("reserve").await;
        let fp1 = fingerprint(&state, "u1", "POST", "/api/instances/x/start", "", Some(b"{}"));
        assert!(matches!(
            reserve(&state, "op-1", "u1", "POST", "/api/instances/x/start", &fp1).unwrap(),
            Reserve::Reserved
        ));
        // 相同编号 + 相同内容：重放
        assert!(matches!(
            reserve(&state, "op-1", "u1", "POST", "/api/instances/x/start", &fp1).unwrap(),
            Reserve::Replay(_)
        ));
        // 相同编号 + 不同内容：冲突
        let fp2 = fingerprint(&state, "u1", "POST", "/api/instances/x/start", "", Some(b"{\"a\":1}"));
        assert!(matches!(
            reserve(&state, "op-1", "u1", "POST", "/api/instances/x/start", &fp2).unwrap(),
            Reserve::Conflict
        ));
        // 相同编号 + 其他账户：拒绝
        assert!(matches!(
            reserve(&state, "op-1", "u2", "POST", "/api/instances/x/start", &fp1).unwrap(),
            Reserve::PrincipalConflict
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn completed_operation_keeps_response_and_lookup_filters() {
        let (state, dir) = test_state("complete").await;
        let fp = fingerprint(&state, "u1", "POST", "/api/instances", "", Some(b"{}"));
        let _ = reserve(&state, "op-2", "u1", "POST", "/api/instances", &fp).unwrap();
        complete(&state, "op-2", 200, Some(serde_json::json!({"ok": true})), Some("job-9".into()));
        let op = lookup(&state, "op-2", "u1", false).unwrap();
        assert_eq!(op.status, "done");
        assert!(op.durable, "成功落盘后应标记 durable");
        assert_eq!(op.job_id.as_deref(), Some("job-9"));
        assert_eq!(op.response.unwrap()["ok"], true);
        // 其他账户不可见
        assert!(lookup(&state, "op-2", "u2", false).is_none());
        assert!(lookup(&state, "op-2", "admin", true).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pending_operations_become_interrupted_after_restart() {
        let (state, dir) = test_state("restart").await;
        let fp = fingerprint(&state, "u1", "POST", "/api/x", "", None);
        let _ = reserve(&state, "op-3", "u1", "POST", "/api/x", &fp).unwrap();
        // 释放账户存储文件锁，模拟重启
        drop(state);
        let cfg = PanelConfig {
            data_dir: dir.to_string_lossy().to_string(),
            token: "test".into(),
            ..Default::default()
        };
        let restarted = AppState::new(cfg).await.unwrap();
        let op = lookup(&restarted, "op-3", "u1", false).unwrap();
        assert_eq!(op.status, "interrupted");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn storage_failure_rejects_reserve_without_side_effect() {
        let (state, dir) = test_state("storage").await;
        // 用目录占位 operations.json，使 rename 失败
        std::fs::create_dir_all(dir.join("tasks/operations.json")).unwrap();
        let fp = fingerprint(&state, "u1", "POST", "/api/x", "", Some(b"{}"));
        let err = reserve(&state, "op-fail", "u1", "POST", "/api/x", &fp).unwrap_err();
        assert!(matches!(err, ReserveError::Storage(_)));
        // 内存登记已回滚
        assert!(lookup(&state, "op-fail", "u1", false).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn corrupt_ledger_fails_closed() {
        let (state, dir) = test_state("corrupt").await;
        drop(state);
        std::fs::write(dir.join("tasks/operations.json"), b"{not json").unwrap();
        let cfg = PanelConfig {
            data_dir: dir.to_string_lossy().to_string(),
            token: "test".into(),
            ..Default::default()
        };
        assert!(AppState::new(cfg).await.is_err(), "损坏账本必须拒绝启动");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_never_evicts_pending() {
        let mut map: HashMap<String, Operation> = HashMap::new();
        let ts = now_iso();
        let make = |id: &str, status: &str| Operation {
            id: id.to_string(),
            principal: "u".into(),
            method: "POST".into(),
            route: "/api/x".into(),
            fingerprint: "f".into(),
            status: status.into(),
            status_code: if status == "pending" { 0 } else { 200 },
            job_id: None,
            response: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            finished_at: if status == "pending" { None } else { Some(ts.clone()) },
            durable: status != "pending",
        };
        for i in 0..50 {
            map.insert(format!("pending-{i}"), make(&format!("pending-{i}"), "pending"));
        }
        for i in 0..(MAX_OPERATIONS + 10) {
            map.insert(format!("done-{i}"), make(&format!("done-{i}"), "done"));
        }
        prune(&mut map);
        let pending = map.values().filter(|op| op.status == "pending").count();
        assert_eq!(pending, 50, "pending 记录不得被裁剪");
        let terminal = map.values().filter(|op| op.status != "pending").count();
        assert!(terminal <= MAX_OPERATIONS, "终态记录应有界: {terminal}");
    }

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 测试用例 1
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
