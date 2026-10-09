//! 用户管理：OP / 白名单 / 封禁玩家 / 封禁 IP
//!
//! 服务器运行中通过控制台命令操作（实时生效）；
//! 未运行时直接读写 ops.json / whitelist.json / banned-players.json / banned-ips.json。

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub fn read_array(dir: &Path, file: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(file))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// 严格读取玩家列表 JSON：文件缺失视为空数组；读取失败、JSON 非法或顶层不是数组
/// 一律返回错误。自动同步据此拒绝在无法确认现状时覆盖白名单文件。
pub(crate) fn read_array_strict(dir: &Path, file: &str) -> Result<Vec<Value>, String> {
    let path = dir.join(file);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("解析 {} 失败: {e}", path.display()))?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| format!("{} 顶层不是数组", path.display()))
}

/// 原子写入玩家列表 JSON：先写同目录唯一临时文件再 rename 替换，
/// 写入中断或并发写者不会留下半文件，也不会互相覆盖临时文件。
pub(crate) async fn write_array_atomic(dir: &Path, file: &str, arr: &[Value]) -> ApiResult<()> {
    let s = serde_json::to_string_pretty(arr)?;
    let tmp = dir.join(format!("{file}.tmp-{}", uuid::Uuid::new_v4()));
    let result = async {
        tokio::fs::write(&tmp, s).await?;
        tokio::fs::rename(&tmp, dir.join(file)).await
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

/// 合法 Minecraft 玩家名：1-16 位字母、数字或下划线。
pub(crate) fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty() && bytes.len() <= 16 && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

async fn write_array(dir: &Path, file: &str, arr: &[Value]) -> ApiResult<()> {
    let s = serde_json::to_string_pretty(arr)?;
    tokio::fs::write(dir.join(file), s).await?;
    Ok(())
}

fn name_of(entry: &Value) -> Option<&str> {
    entry.get("name").and_then(|v| v.as_str())
}

fn contains_name(arr: &[Value], name: &str) -> bool {
    arr.iter()
        .any(|e| name_of(e).map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false))
}

/// 在现有 JSON（ops/whitelist/banned/usercache）中查找玩家已知的 UUID
fn find_uuid_by_name(dir: &Path, name: &str) -> Option<String> {
    for f in ["ops.json", "whitelist.json", "banned-players.json", "usercache.json"] {
        for e in read_array(dir, f) {
            if name_of(&e).map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false) {
                if let Some(u) = e.get("uuid").and_then(|v| v.as_str()) {
                    return Some(u.to_string());
                }
            }
        }
    }
    None
}

fn dashed_uuid(hex: &str) -> String {
    let h: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if h.len() != 32 {
        return hex.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]
    )
}

/// 离线模式 UUID：MD5("OfflinePlayer:<name>") 的 v3 UUID（与原版算法一致）
pub(crate) fn offline_uuid(name: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(format!("OfflinePlayer:{name}"));
    let mut hash = hasher.finalize();
    hash[6] = (hash[6] & 0x0f) | 0x30;
    hash[8] = (hash[8] & 0x3f) | 0x80;
    let s: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    dashed_uuid(&s)
}

pub(crate) async fn mojang_uuid(state: &AppState, name: &str) -> Result<String, String> {
    let url = format!("https://api.mojang.com/users/profiles/minecraft/{name}");
    let resp = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    let hex = v
        .get("id")
        .and_then(|x| x.as_str())
        .ok_or("响应缺少 id 字段")?;
    Ok(dashed_uuid(hex))
}

/// 解析玩家 UUID：本地缓存 → Mojang API → 离线 UUID（带警告）
async fn resolve_uuid(
    state: &AppState,
    dir: &Path,
    name: &str,
) -> Result<(String, Option<String>), String> {
    if let Some(u) = find_uuid_by_name(dir, name) {
        return Ok((u, None));
    }
    match mojang_uuid(state, name).await {
        Ok(u) => Ok((u, None)),
        Err(_) => Ok((
            offline_uuid(name),
            Some(format!(
                "无法连接 Mojang API 获取「{name}」的正版 UUID，已按离线模式 UUID 写入。若服务器开启正版验证，请启动服务器后改用命令操作。"
            )),
        )),
    }
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S %z").to_string()
}

pub async fn add_op(state: &AppState, dir: &Path, name: &str) -> ApiResult<Option<String>> {
    let mut arr = read_array(dir, "ops.json");
    if contains_name(&arr, name) {
        return Ok(Some("该玩家已在 OP 列表中".into()));
    }
    let (uuid, warn) = resolve_uuid(state, dir, name)
        .await
        .map_err(ApiError::bad_request)?;
    arr.push(json!({
        "uuid": uuid,
        "name": name,
        "level": 4,
        "bypassesPlayerLimit": false,
    }));
    write_array(dir, "ops.json", &arr).await?;
    Ok(warn)
}

pub async fn add_whitelist(state: &AppState, dir: &Path, name: &str) -> ApiResult<Option<String>> {
    let mut arr = read_array(dir, "whitelist.json");
    if contains_name(&arr, name) {
        return Ok(Some("该玩家已在白名单中".into()));
    }
    let (uuid, warn) = resolve_uuid(state, dir, name)
        .await
        .map_err(ApiError::bad_request)?;
    arr.push(json!({ "uuid": uuid, "name": name }));
    write_array(dir, "whitelist.json", &arr).await?;
    Ok(warn)
}

pub async fn add_ban(
    state: &AppState,
    dir: &Path,
    name: &str,
    reason: Option<&str>,
) -> ApiResult<Option<String>> {
    let mut arr = read_array(dir, "banned-players.json");
    if contains_name(&arr, name) {
        return Ok(Some("该玩家已被封禁".into()));
    }
    let (uuid, warn) = resolve_uuid(state, dir, name)
        .await
        .map_err(ApiError::bad_request)?;
    arr.push(json!({
        "uuid": uuid,
        "name": name,
        "created": now_stamp(),
        "source": "Server",
        "expires": "forever",
        "reason": reason.unwrap_or("Banned by an operator"),
    }));
    write_array(dir, "banned-players.json", &arr).await?;
    Ok(warn)
}

pub async fn add_ban_ip(dir: &Path, ip: &str, reason: Option<&str>) -> ApiResult<Option<String>> {
    let mut arr = read_array(dir, "banned-ips.json");
    if arr
        .iter()
        .any(|e| e.get("ip").and_then(|v| v.as_str()) == Some(ip))
    {
        return Ok(Some("该 IP 已被封禁".into()));
    }
    arr.push(json!({
        "ip": ip,
        "created": now_stamp(),
        "source": "Server",
        "expires": "forever",
        "reason": reason.unwrap_or("Banned by an operator"),
    }));
    write_array(dir, "banned-ips.json", &arr).await?;
    Ok(None)
}

/// 从指定文件中移除条目（按 name 或 ip 匹配），返回是否确实存在并被移除
pub async fn remove_entry(dir: &Path, file: &str, key: &str, value: &str) -> ApiResult<bool> {
    let mut arr = read_array(dir, file);
    let before = arr.len();
    arr.retain(|e| {
        let matches = match key {
            "ip" => e.get("ip").and_then(|v| v.as_str()) == Some(value),
            _ => e
                .get(key)
                .and_then(|v| v.as_str())
                .map(|n| n.eq_ignore_ascii_case(value))
                .unwrap_or(false),
        };
        !matches
    });
    let removed = arr.len() != before;
    if removed {
        write_array(dir, file, &arr).await?;
    }
    Ok(removed)
}
