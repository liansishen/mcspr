//! 白名单自动同步：按有效实例授权协调每个实例的 `whitelist.json`。
//!
//! 约束：
//! - 输入为纯 `DesiredSnapshot`，不依赖认证实现。
//! - 每个实例用 `whitelist-sync.json` 记录条目来源、代号与待重试成员。
//! - 停止实例原子写文件；运行实例下命令后必须校验文件真正生效，超时记为待重试。
//! - 损坏的白名单只报错不覆盖；移除优先于新增；人工条目永不自动删除。
//! - 白名单任何变更前，来源登记必须先落盘，避免崩溃后丢失所有权。

use super::{users, InstanceRuntime, Status, WhitelistIdentity};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// 实例级旁路状态文件名
const SIDECAR: &str = "whitelist-sync.json";
const SCHEMA_VERSION: u32 = 1;
/// 运行实例命令下发后等待白名单文件生效的最长时间
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);
const VERIFY_INTERVAL: Duration = Duration::from_millis(200);

// ---------- 期望快照（纯数据，父模块负责从认证存储构造） ----------

/// 单个实例上期望存在的成员（账户 → 游戏名）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredMember {
    pub user_id: String,
    pub name: String,
}

/// 一个账户的有效授权：管理员覆盖全部实例，普通用户按 `instance_ids`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredGrant {
    pub user_id: String,
    pub name: String,
    #[serde(default)]
    pub instance_ids: Vec<String>,
    #[serde(default)]
    pub is_admin: bool,
}

/// 一次全局同步的输入快照。`revision` 来自账户存储的单调修订号，用作同步代号。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredSnapshot {
    pub revision: u64,
    #[serde(default)]
    pub grants: Vec<DesiredGrant>,
}

impl DesiredSnapshot {
    /// 由认证存储的 `(user_id, minecraft_name, instance_ids, is_admin)` 授权列表构造快照。
    pub fn from_grants(
        revision: u64,
        grants: Vec<(String, String, Vec<String>, bool)>,
    ) -> Self {
        Self {
            revision,
            grants: grants
                .into_iter()
                .map(|(user_id, name, instance_ids, is_admin)| DesiredGrant {
                    user_id,
                    name,
                    instance_ids,
                    is_admin,
                })
                .collect(),
        }
    }
}

/// 计算某实例上期望存在的成员集合：管理员覆盖全部实例，普通用户仅限被分配的实例。
pub fn effective_members(snapshot: &DesiredSnapshot, instance_id: &str) -> Vec<DesiredMember> {
    let mut seen_user = BTreeSet::new();
    let mut out = Vec::new();
    for g in &snapshot.grants {
        let name = g.name.trim();
        if name.is_empty() {
            continue;
        }
        let in_scope = g.is_admin || g.instance_ids.iter().any(|i| i == instance_id);
        if !in_scope {
            continue;
        }
        if !seen_user.insert(g.user_id.clone()) {
            continue;
        }
        out.push(DesiredMember {
            user_id: g.user_id.clone(),
            name: name.to_string(),
        });
    }
    out
}

// ---------- 结果类型 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// 从未同步且无面板管理条目，未创建状态文件
    Idle,
    /// 期望集合已完全生效
    Applied,
    /// 部分变更生效，仍有成员待重试
    Partial,
    /// 未确认任何变更（延后 / 全部待重试）
    Pending,
    /// 白名单或状态文件损坏 / IO 错误，未覆盖文件
    Error,
    /// 实例是代理服务端，不具备实例白名单能力
    Unsupported,
    /// 转发配置不明确，需要实例级身份模式覆盖
    NeedsOverride,
    /// 代号陈旧，忽略本次同步
    Skipped,
}

impl SyncStatus {
    fn as_str(self) -> &'static str {
        match self {
            SyncStatus::Idle => "idle",
            SyncStatus::Applied => "applied",
            SyncStatus::Partial => "partial",
            SyncStatus::Pending => "pending",
            SyncStatus::Error => "error",
            SyncStatus::Unsupported => "unsupported",
            SyncStatus::NeedsOverride => "needs_override",
            SyncStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMember {
    pub user_id: String,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncOutcome {
    pub instance_id: String,
    pub status: SyncStatus,
    pub generation: u64,
    pub applied_generation: u64,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub pending: Vec<PendingMember>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncAllReport {
    pub revision: u64,
    pub outcomes: Vec<SyncOutcome>,
    /// 全部实例均为 applied / idle / skipped 时为 true
    pub ok: bool,
}

// ---------- 持久化状态 ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedEntry {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    uuid: String,
    /// panel：面板管理，可被撤权移除；manual_dup：人工保留，永不自动删除
    #[serde(default)]
    origin: String,
    /// 解析该 UUID 时使用的身份模式
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    #[serde(default = "default_schema")]
    schema_version: u32,
    #[serde(default)]
    status: String,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    applied_generation: u64,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    desired: Vec<DesiredMember>,
    #[serde(default)]
    managed: BTreeMap<String, ManagedEntry>,
    /// 人工保留的游戏名（小写）：即使撤权也保留，避免误删管理员手动条目
    #[serde(default)]
    manual_retained: BTreeSet<String>,
    #[serde(default)]
    pending: Vec<PendingMember>,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status: SyncStatus::Idle.as_str().to_string(),
            generation: 0,
            applied_generation: 0,
            attempts: 0,
            last_error: None,
            updated_at: String::new(),
            desired: Vec::new(),
            managed: BTreeMap::new(),
            manual_retained: BTreeSet::new(),
            pending: Vec::new(),
        }
    }
}

struct ResolvedMember {
    user_id: String,
    name: String,
    uuid: String,
}

// ---------- 身份模式判定 ----------

enum IdentityDecision {
    Mode(WhitelistIdentity),
    Unsupported(String),
    NeedsOverride(String),
}

fn identity_decision(meta: &super::InstanceMeta, dir: &Path) -> IdentityDecision {
    let jar = meta.jar.clone().unwrap_or_default().to_ascii_lowercase();
    if jar.contains("velocity") || jar.contains("bungeecord") || jar.contains("waterfall") {
        return IdentityDecision::Unsupported(
            "该实例是代理服务端（Velocity/BungeeCord/Waterfall），不提供实例白名单能力".into(),
        );
    }
    if let Some(mode) = meta.whitelist_identity {
        return IdentityDecision::Mode(mode);
    }
    let props = std::fs::read_to_string(dir.join("server.properties")).unwrap_or_default();
    let mut online: Option<bool> = None;
    let mut bungee = false;
    for line in props.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "online-mode" => online = Some(v.trim().eq_ignore_ascii_case("true")),
                "bungeecord" => bungee = v.trim().eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
    }
    if bungee {
        return IdentityDecision::NeedsOverride(
            "检测到 bungeecord 转发，online-mode 不足以确定白名单身份，请在实例设置中显式指定白名单身份模式".into(),
        );
    }
    IdentityDecision::Mode(if online.unwrap_or(true) {
        WhitelistIdentity::Online
    } else {
        WhitelistIdentity::Offline
    })
}

#[cfg(test)]
static ONLINE_UUID_OVERRIDE: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn online_override() -> Option<std::collections::HashMap<String, String>> {
    ONLINE_UUID_OVERRIDE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// 正版 UUID 解析：正版模式查询失败时绝不回退为离线 UUID。
async fn resolve_online(state: &AppState, name: &str) -> Result<String, String> {
    #[cfg(test)]
    {
        if let Some(map) = online_override() {
            return map
                .get(name)
                .cloned()
                .ok_or_else(|| format!("测试环境未提供「{name}」的正版 UUID"));
        }
    }
    users::mojang_uuid(state, name).await
}

// ---------- 核心协调 ----------

/// 协调单个实例到给定的期望成员集合。
///
/// `generation` 为调用方单调代号（通常为账户修订号）。小于已持久化代号
/// （`generation` 与 `applied_generation` 的较大者）时直接跳过，
/// 避免迟到的旧快照回滚更新的期望集合。
pub async fn reconcile_instance(
    state: &AppState,
    rt: &Arc<InstanceRuntime>,
    desired: &[DesiredMember],
    generation: u64,
) -> SyncOutcome {
    let id = rt.meta.read().await.id.clone();
    let dir = rt.dir.clone();
    // 实例级串行锁：文件写入与命令下发互斥，避免并发同步互相覆盖
    let _serial = rt.whitelist_sync_lock.lock().await;

    let mut st = match load_state(&dir).await {
        Ok(s) => s,
        Err(e) => {
            return SyncOutcome {
                instance_id: id,
                status: SyncStatus::Error,
                generation,
                applied_generation: 0,
                added: Vec::new(),
                removed: Vec::new(),
                pending: Vec::new(),
                error: Some(format!("同步状态文件损坏，已拒绝改动白名单：{e}")),
                attempts: 0,
            };
        }
    };

    // 陈旧代号：不覆盖已持久化的期望集合，也不做任何改动
    if generation < st.generation.max(st.applied_generation) {
        return build_outcome(&id, SyncStatus::Skipped, generation, &st, vec![], vec![], None);
    }

    if desired.is_empty() && st.managed.is_empty() && st.manual_retained.is_empty() {
        return build_outcome(&id, SyncStatus::Idle, generation, &st, vec![], vec![], None);
    }

    st.desired = desired.to_vec();
    st.generation = generation;
    st.attempts = st.attempts.saturating_add(1);

    let meta = rt.meta.read().await.clone();
    let mode = match identity_decision(&meta, &dir) {
        IdentityDecision::Mode(m) => m,
        IdentityDecision::Unsupported(reason) => {
            return finalize_no_change(&dir, &mut st, &id, SyncStatus::Unsupported, generation, Some(reason))
                .await;
        }
        IdentityDecision::NeedsOverride(reason) => {
            return finalize_no_change(&dir, &mut st, &id, SyncStatus::NeedsOverride, generation, Some(reason))
                .await;
        }
    };

    // 严格读取现有白名单；损坏时只报错，绝不覆盖
    let current = match users::read_array_strict(&dir, "whitelist.json") {
        Ok(v) => v,
        Err(e) => {
            st.status = SyncStatus::Error.as_str().to_string();
            st.last_error = Some(e.clone());
            st.updated_at = now();
            let _ = save_state(&dir, &st).await;
            return build_outcome(&id, SyncStatus::Error, generation, &st, vec![], vec![], Some(e));
        }
    };
    let current_by_name = index_by_name(&current);

    // 解析期望成员为 (name, uuid)
    let mut resolved: BTreeMap<String, ResolvedMember> = BTreeMap::new();
    let mut resolution_pending: Vec<PendingMember> = Vec::new();
    for m in dedup_desired(desired) {
        if !users::valid_name(&m.name) {
            resolution_pending.push(PendingMember {
                user_id: m.user_id.clone(),
                name: m.name.clone(),
                reason: "游戏名不合法（仅限字母、数字、下划线，1-16 字符）".into(),
            });
            continue;
        }
        let key = m.name.to_lowercase();
        let uuid = match mode {
            WhitelistIdentity::Offline => Some(users::offline_uuid(&m.name)),
            WhitelistIdentity::Online => {
                let reused = st
                    .managed
                    .get(&key)
                    .filter(|e| {
                        e.origin == "panel"
                            && e.mode.as_deref() == Some("online")
                            && e.name.eq_ignore_ascii_case(&m.name)
                            && !e.uuid.is_empty()
                    })
                    .map(|e| e.uuid.clone());
                match reused {
                    Some(u) => Some(u),
                    None => match resolve_online(state, &m.name).await {
                        Ok(u) => Some(u),
                        Err(e) => {
                            resolution_pending.push(PendingMember {
                                user_id: m.user_id.clone(),
                                name: m.name.clone(),
                                reason: e,
                            });
                            None
                        }
                    },
                }
            }
        };
        if let Some(uuid) = uuid {
            resolved.insert(
                key,
                ResolvedMember {
                    user_id: m.user_id.clone(),
                    name: m.name.clone(),
                    uuid,
                },
            );
        }
    }

    let panel_managed: BTreeSet<String> = st
        .managed
        .iter()
        .filter(|(_, e)| e.origin == "panel")
        .map(|(k, _)| k.clone())
        .collect();
    let manual_dup: BTreeSet<String> = st
        .managed
        .iter()
        .filter(|(_, e)| e.origin == "manual_dup")
        .map(|(k, _)| k.clone())
        .collect();

    // 需要替换的面板条目（UUID 变化，例如正版解析修正）
    let mut to_replace: BTreeSet<String> = BTreeSet::new();
    for (key, r) in &resolved {
        if !panel_managed.contains(key) {
            continue;
        }
        if let Some(v) = current_by_name.get(key) {
            let cur = v.get("uuid").and_then(|x| x.as_str()).unwrap_or("");
            if !cur.is_empty() && !cur.eq_ignore_ascii_case(&r.uuid) {
                to_replace.insert(key.clone());
            }
        }
    }

    // 移除：仅面板管理且不再期望（或需替换）的条目；安全优先，先移除后新增
    let mut removals: Vec<(String, String)> = Vec::new();
    for key in &panel_managed {
        if resolved.contains_key(key) && !to_replace.contains(key) {
            continue;
        }
        if let Some(v) = current_by_name.get(key) {
            let name = entry_name(v)
                .map(|s| s.to_string())
                .or_else(|| st.managed.get(key).map(|e| e.name.clone()))
                .unwrap_or_default();
            removals.push((key.clone(), name));
        }
    }

    // 现有但未被面板管理的条目若被期望，则标记为人工保留（人工重复条目）
    for key in resolved.keys() {
        if current_by_name.contains_key(key) && !panel_managed.contains(key) {
            st.manual_retained.insert(key.clone());
        }
    }

    // 新增：期望但当前不存在，或需替换的条目
    let mut additions: Vec<(String, String)> = Vec::new();
    for (key, r) in &resolved {
        if !current_by_name.contains_key(key) || to_replace.contains(key) {
            additions.push((r.name.clone(), r.uuid.clone()));
        }
    }

    // 白名单变更前先落盘来源登记（意图日志）：崩溃或白名单写失败时，
    // 面板管理条目的所有权不会丢失，已撤销用户不会被误判为人工条目。
    let intent_managed = build_managed_plan(&resolved, &st, &manual_dup, &current_by_name, &removals, mode);
    st.managed = intent_managed.clone();
    st.pending = dedup_pending(resolution_pending.clone());
    st.status = "applying".to_string();
    st.last_error = None;
    st.applied_generation = generation;
    st.updated_at = now();
    if let Err(e) = save_state(&dir, &st).await {
        return build_outcome(
            &id,
            SyncStatus::Error,
            generation,
            &st,
            vec![],
            vec![],
            Some(format!("同步状态无法落盘，已拒绝改动白名单：{e}")),
        );
    }
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut apply_error: Option<String> = None;
    let mut final_entries: Vec<Value> = current.clone();

    let status = *rt.status.lock().await;
    if matches!(status, Status::Starting | Status::Stopping) {
        apply_error = Some("实例正在启动或停止，已延后白名单同步".into());
    } else if status == Status::Running {
        // 先移除（安全优先）
        let remove_names: Vec<String> = removals.iter().map(|(_, n)| n.clone()).collect();
        for name in &remove_names {
            if let Err(e) = super::process::send_command(rt, &format!("whitelist remove {name}")).await {
                apply_error = Some(format!("发送 whitelist remove {name} 失败: {e}"));
                break;
            }
        }
        if apply_error.is_none() {
            match verify_convergence(&dir, &[], &remove_names, VERIFY_TIMEOUT).await {
                Ok(()) => removed = remove_names,
                Err(e) => apply_error = Some(format!("白名单移除未在超时内生效: {e}")),
            }
        }
        if apply_error.is_none() {
            let add_names: Vec<String> = additions.iter().map(|(n, _)| n.clone()).collect();
            for name in &add_names {
                if let Err(e) = super::process::send_command(rt, &format!("whitelist add {name}")).await {
                    apply_error = Some(format!("发送 whitelist add {name} 失败: {e}"));
                    break;
                }
            }
            if apply_error.is_none() {
                match verify_convergence(&dir, &add_names, &[], VERIFY_TIMEOUT).await {
                    Ok(()) => added = add_names,
                    Err(e) => apply_error = Some(format!("白名单新增未在超时内生效: {e}")),
                }
            }
        }
        final_entries = users::read_array_strict(&dir, "whitelist.json").unwrap_or_else(|_| current.clone());
    } else {
        // 停止：一次性原子写入
        if !removals.is_empty() || !additions.is_empty() {
            let mut next = current.clone();
            let remove_keys: BTreeSet<String> = removals.iter().map(|(k, _)| k.clone()).collect();
            next.retain(|e| {
                entry_name(e)
                    .map(|n| !remove_keys.contains(&n.to_lowercase()))
                    .unwrap_or(true)
            });
            for (name, uuid) in &additions {
                next.push(json!({ "uuid": uuid, "name": name }));
            }
            match users::write_array_atomic(&dir, "whitelist.json", &next).await {
                Ok(()) => {
                    removed = removals.iter().map(|(_, n)| n.clone()).collect();
                    added = additions.iter().map(|(n, _)| n.clone()).collect();
                    final_entries = next;
                }
                Err(e) => apply_error = Some(format!("写入白名单失败: {e}")),
            }
        }
    }

    let final_by_name = index_by_name(&final_entries);

    // 已确认移除的面板条目从来源登记删除；未确认的保留以便下次重试。
    let mut managed = intent_managed;
    for name in &removed {
        let key = name.to_lowercase();
        if !resolved.contains_key(&key) {
            managed.remove(&key);
        }
    }

    // 待重试集合：解析失败 + 未确认的变更
    let mut final_pending = resolution_pending.clone();
    if matches!(status, Status::Starting | Status::Stopping) {
        for m in dedup_desired(desired) {
            final_pending.push(PendingMember {
                user_id: m.user_id,
                name: m.name,
                reason: "实例正在启动或停止，已延后同步".into(),
            });
        }
    } else if let Some(err) = &apply_error {
        for (key, r) in &resolved {
            if !final_by_name.contains_key(key) {
                final_pending.push(PendingMember {
                    user_id: r.user_id.clone(),
                    name: r.name.clone(),
                    reason: err.clone(),
                });
            }
        }
        for (key, name) in &removals {
            if final_by_name.contains_key(key) {
                final_pending.push(PendingMember {
                    user_id: String::new(),
                    name: name.clone(),
                    reason: err.clone(),
                });
            }
        }
    }
    let final_pending = dedup_pending(final_pending);

    let status = if apply_error.is_some() || !final_pending.is_empty() {
        if added.is_empty() && removed.is_empty() {
            SyncStatus::Pending
        } else {
            SyncStatus::Partial
        }
    } else {
        SyncStatus::Applied
    };

    st.status = status.as_str().to_string();
    st.last_error = apply_error.clone();
    st.pending = final_pending;
    st.managed = managed;
    st.applied_generation = generation;
    st.updated_at = now();
    if let Err(e) = save_state(&dir, &st).await {
        return build_outcome(
            &id,
            SyncStatus::Error,
            generation,
            &st,
            added,
            removed,
            Some(format!("同步状态更新落盘失败（来源登记已持久化）: {e}")),
        );
    }
    build_outcome(&id, status, generation, &st, added, removed, apply_error)
}

/// 生命周期钩子：实例新建 / 启动 / 更新 / 重装 / 恢复后重新协调已持久化的期望集合。
///
/// 返回 `None` 表示该实例从未同步过（无期望集合），无需处理。
pub async fn reconcile_lifecycle(
    state: &AppState,
    rt: &Arc<InstanceRuntime>,
) -> Option<SyncOutcome> {
    let dir = rt.dir.clone();
    let st = load_state(&dir).await.ok()?;
    if st.desired.is_empty() && st.managed.is_empty() {
        return None;
    }
    let generation = st.generation.max(st.applied_generation);
    Some(reconcile_instance(state, rt, &st.desired, generation).await)
}

/// 手动新增白名单条目时调用：记录人工保留标记（自行获取实例锁）。
///
/// 若该条目此前由面板管理，则改为人工保留，避免后续撤权时被自动删除。
pub async fn mark_manual_retained(rt: &Arc<InstanceRuntime>, name: &str) -> Result<(), String> {
    let _serial = rt.whitelist_sync_lock.lock().await;
    mark_manual_retained_locked(&rt.dir, name).await
}

/// 同 [`mark_manual_retained`]，但由调用方持有实例级锁，避免重复加锁死锁。
pub(crate) async fn mark_manual_retained_locked(dir: &Path, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(());
    }
    let mut st = load_state(dir).await?;
    let key = name.to_lowercase();
    st.manual_retained.insert(key.clone());
    if let Some(e) = st.managed.get_mut(&key) {
        e.origin = "manual_dup".into();
    }
    st.updated_at = now();
    save_state(dir, &st).await
}

/// 读取某实例的同步状态（供 `GET /instances/{id}/whitelist-sync`）。
pub async fn status_view(rt: &Arc<InstanceRuntime>) -> Value {
    let dir = rt.dir.clone();
    match load_state(&dir).await {
        Ok(st) => json!({
            "status": st.status,
            "generation": st.generation,
            "applied_generation": st.applied_generation,
            "attempts": st.attempts,
            "last_error": st.last_error,
            "updated_at": st.updated_at,
            "pending": st.pending,
            "desired": st.desired,
            "manual_retained": st.manual_retained.iter().cloned().collect::<Vec<_>>(),
            "managed": st.managed.iter().map(|(key, e)| json!({
                "key": key,
                "name": e.name,
                "user_id": e.user_id,
                "uuid": e.uuid,
                "origin": e.origin,
                "mode": e.mode,
            })).collect::<Vec<_>>(),
        }),
        Err(e) => json!({ "status": "error", "error": e }),
    }
}

/// 对全部实例执行一次同步。父模块用认证存储的已批准授权构造 `snapshot` 后调用。
pub async fn sync_all(state: &AppState, snapshot: &DesiredSnapshot) -> SyncAllReport {
    let instances: Vec<Arc<InstanceRuntime>> = state.instances.read().await.values().cloned().collect();
    let mut outcomes = Vec::new();
    for rt in instances {
        let id = rt.meta.read().await.id.clone();
        let desired = effective_members(snapshot, &id);
        outcomes.push(reconcile_instance(state, &rt, &desired, snapshot.revision).await);
    }
    let ok = outcomes
        .iter()
        .all(|o| matches!(o.status, SyncStatus::Applied | SyncStatus::Idle | SyncStatus::Skipped));
    SyncAllReport {
        revision: snapshot.revision,
        outcomes,
        ok,
    }
}

// ---------- 运行实例的生效校验 ----------

/// 轮询白名单文件，直到期望出现 / 消失的成员都已反映；超时返回错误。
async fn verify_convergence(
    dir: &Path,
    expect_present: &[String],
    expect_absent: &[String],
    timeout: Duration,
) -> Result<(), String> {
    if expect_present.is_empty() && expect_absent.is_empty() {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = "白名单文件尚未反映期望变更".to_string();
    loop {
        match users::read_array_strict(dir, "whitelist.json") {
            Ok(entries) => {
                let names: BTreeSet<String> = entries
                    .iter()
                    .filter_map(entry_name)
                    .map(|n| n.to_lowercase())
                    .collect();
                let present_ok = expect_present
                    .iter()
                    .all(|n| names.contains(&n.to_lowercase()));
                let absent_ok = expect_absent
                    .iter()
                    .all(|n| !names.contains(&n.to_lowercase()));
                if present_ok && absent_ok {
                    return Ok(());
                }
            }
            Err(e) => last = e,
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(last);
        }
        tokio::time::sleep(VERIFY_INTERVAL).await;
    }
}

// ---------- 辅助 ----------

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn entry_name(v: &Value) -> Option<&str> {
    v.get("name").and_then(|n| n.as_str())
}

fn index_by_name(entries: &[Value]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .filter_map(|e| entry_name(e).map(|n| (n.to_lowercase(), e.clone())))
        .collect()
}

fn dedup_desired(desired: &[DesiredMember]) -> Vec<DesiredMember> {
    let mut seen_user = BTreeSet::new();
    let mut seen_name = BTreeSet::new();
    let mut out = Vec::new();
    for m in desired {
        let key = m.name.trim().to_lowercase();
        if m.user_id.is_empty() || key.is_empty() {
            continue;
        }
        if !seen_user.insert(m.user_id.clone()) {
            continue;
        }
        if !seen_name.insert(key) {
            continue;
        }
        out.push(DesiredMember {
            user_id: m.user_id.clone(),
            name: m.name.trim().to_string(),
        });
    }
    out
}

fn dedup_pending(list: Vec<PendingMember>) -> Vec<PendingMember> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in list {
        if seen.insert((p.user_id.clone(), p.name.to_lowercase())) {
            out.push(p);
        }
    }
    out
}

/// 规划来源登记：期望成员 + 人工保留成员 + 未确认移除的面板条目。
///
/// 在任何白名单改动前持久化，保证崩溃后所有权不丢失。
fn build_managed_plan(
    resolved: &BTreeMap<String, ResolvedMember>,
    st: &StateFile,
    manual_dup: &BTreeSet<String>,
    current_by_name: &BTreeMap<String, Value>,
    removals: &[(String, String)],
    mode: WhitelistIdentity,
) -> BTreeMap<String, ManagedEntry> {
    let mut managed: BTreeMap<String, ManagedEntry> = BTreeMap::new();
    for (key, r) in resolved {
        let manual = st.manual_retained.contains(key) || manual_dup.contains(key);
        let origin = if manual { "manual_dup" } else { "panel" };
        let uuid = current_by_name
            .get(key)
            .and_then(|e| e.get("uuid").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .unwrap_or_else(|| r.uuid.clone());
        let name = current_by_name
            .get(key)
            .and_then(entry_name)
            .map(|s| s.to_string())
            .unwrap_or_else(|| r.name.clone());
        managed.insert(
            key.clone(),
            ManagedEntry {
                user_id: r.user_id.clone(),
                name,
                uuid,
                origin: origin.into(),
                mode: Some(mode.as_str().into()),
            },
        );
    }
    for key in &st.manual_retained {
        if managed.contains_key(key) {
            continue;
        }
        if let Some(v) = current_by_name.get(key) {
            managed.insert(
                key.clone(),
                ManagedEntry {
                    user_id: String::new(),
                    name: entry_name(v).unwrap_or(key.as_str()).to_string(),
                    uuid: v.get("uuid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    origin: "manual_dup".into(),
                    mode: None,
                },
            );
        }
    }
    for (key, _) in removals {
        if managed.contains_key(key) {
            continue;
        }
        if let Some(e) = st.managed.get(key) {
            managed.insert(key.clone(), e.clone());
        }
    }
    managed
}

fn build_outcome(
    id: &str,
    status: SyncStatus,
    generation: u64,
    st: &StateFile,
    added: Vec<String>,
    removed: Vec<String>,
    error: Option<String>,
) -> SyncOutcome {
    SyncOutcome {
        instance_id: id.to_string(),
        status,
        generation,
        applied_generation: st.applied_generation,
        added,
        removed,
        pending: st.pending.clone(),
        error,
        attempts: st.attempts,
    }
}

async fn finalize_no_change(
    dir: &Path,
    st: &mut StateFile,
    id: &str,
    status: SyncStatus,
    generation: u64,
    error: Option<String>,
) -> SyncOutcome {
    st.status = status.as_str().to_string();
    st.last_error = error.clone();
    st.updated_at = now();
    let _ = save_state(dir, st).await;
    build_outcome(id, status, generation, st, vec![], vec![], error)
}

async fn load_state(dir: &Path) -> Result<StateFile, String> {
    let path = dir.join(SIDECAR);
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StateFile::default()),
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(StateFile::default());
    }
    let parsed: StateFile =
        serde_json::from_str(&raw).map_err(|e| format!("解析 {} 失败: {e}", path.display()))?;
    if parsed.schema_version == 0 || parsed.schema_version > SCHEMA_VERSION {
        return Err(format!(
            "不支持的 whitelist-sync schema_version={}（支持 1..={SCHEMA_VERSION}）",
            parsed.schema_version
        ));
    }
    Ok(parsed)
}

async fn save_state(dir: &Path, st: &StateFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(st).map_err(|e| e.to_string())?;
    let target = dir.join(SIDECAR);
    let tmp = dir.join(format!("{SIDECAR}.tmp-{}", uuid::Uuid::new_v4()));
    let result = async {
        tokio::fs::write(&tmp, json).await?;
        users::remove_symlink_target(&target).await;
        tokio::fs::rename(&tmp, &target).await
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelConfig;
    use crate::instance::InstanceMeta;

    /// 在线模式测试会改写全局覆盖表，串行执行避免互相干扰。
    static ONLINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn set_online_override(map: Option<Vec<(&str, &str)>>) {
        let m = map.map(|v| {
            v.into_iter()
                .map(|(k, val)| (k.to_string(), val.to_string()))
                .collect()
        });
        *ONLINE_UUID_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = m;
    }

    struct Ctx {
        state: AppState,
        rt: Arc<InstanceRuntime>,
        root: std::path::PathBuf,
    }

    impl Drop for Ctx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn setup(tag: &str) -> Ctx {
        let root = std::env::temp_dir().join(format!("mcspr-wlsync-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let state = AppState::new(PanelConfig {
            data_dir: root.join("data").to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
        let inst_dir = root.join("inst");
        std::fs::create_dir_all(&inst_dir).unwrap();
        let meta = InstanceMeta {
            id: "i1".into(),
            name: "inst".into(),
            ..Default::default()
        };
        let rt = InstanceRuntime::new(meta, inst_dir);
        Ctx { state, rt, root }
    }

    fn write_props(ctx: &Ctx, body: &str) {
        std::fs::write(ctx.rt.dir.join("server.properties"), body).unwrap();
    }

    fn members(list: &[(&str, &str)]) -> Vec<DesiredMember> {
        list.iter()
            .map(|(u, n)| DesiredMember {
                user_id: u.to_string(),
                name: n.to_string(),
            })
            .collect()
    }

    fn whitelist_names(rt: &Arc<InstanceRuntime>) -> Vec<String> {
        let arr = users::read_array_strict(&rt.dir, "whitelist.json").unwrap();
        let mut v: Vec<String> = arr.iter().filter_map(entry_name).map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    async fn read_state(rt: &Arc<InstanceRuntime>) -> StateFile {
        load_state(&rt.dir).await.unwrap()
    }

    #[tokio::test]
    async fn stopped_offline_adds_with_offline_uuid() {
        let ctx = setup("offline").await;
        write_props(&ctx, "online-mode=false\n");
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        let arr = users::read_array_strict(&ctx.rt.dir, "whitelist.json").unwrap();
        let e = arr
            .iter()
            .find(|e| entry_name(e) == Some("Steve"))
            .expect("Steve 应写入");
        assert_eq!(e["uuid"], users::offline_uuid("Steve"));
        let st = read_state(&ctx.rt).await;
        assert_eq!(st.managed.get("steve").unwrap().origin, "panel");
    }

    #[tokio::test]
    async fn stopped_online_uses_mojang_uuid() {
        let _g = ONLINE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let ctx = setup("online").await;
        write_props(&ctx, "online-mode=true\n");
        set_online_override(Some(vec![("Steve", "069a79f4-44e9-4726-a5be-fca90e38aaf5")]));
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        set_online_override(None);
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        let arr = users::read_array_strict(&ctx.rt.dir, "whitelist.json").unwrap();
        assert_eq!(arr[0]["uuid"], "069a79f4-44e9-4726-a5be-fca90e38aaf5");
    }

    #[tokio::test]
    async fn online_query_failure_never_falls_back_to_offline() {
        let _g = ONLINE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let ctx = setup("online-fail").await;
        write_props(&ctx, "online-mode=true\n");
        set_online_override(Some(vec![]));
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        set_online_override(None);
        assert_eq!(out.status, SyncStatus::Pending, "{out:?}");
        assert!(!ctx.rt.dir.join("whitelist.json").exists());
        assert_eq!(out.pending.len(), 1);
        assert!(out.pending[0].reason.contains("正版"));
    }

    #[tokio::test]
    async fn manual_entries_are_preserved() {
        let ctx = setup("manual").await;
        write_props(&ctx, "online-mode=false\n");
        std::fs::write(
            ctx.rt.dir.join("whitelist.json"),
            r#"[{"uuid":"manual-alex","name":"Alex"}]"#,
        )
        .unwrap();
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Alex".to_string(), "Steve".to_string()]);
    }

    #[tokio::test]
    async fn manual_duplicate_of_desired_member_is_retained() {
        let ctx = setup("manual-dup").await;
        write_props(&ctx, "online-mode=false\n");
        std::fs::write(
            ctx.rt.dir.join("whitelist.json"),
            r#"[{"uuid":"manual-steve","name":"Steve"}]"#,
        )
        .unwrap();
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        let arr = users::read_array_strict(&ctx.rt.dir, "whitelist.json").unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["uuid"], "manual-steve");
        let st = read_state(&ctx.rt).await;
        assert_eq!(st.managed.get("steve").unwrap().origin, "manual_dup");
        assert!(st.manual_retained.contains("steve"));

        // 撤权后人工条目仍保留
        let out = reconcile_instance(&ctx.state, &ctx.rt, &[], 2).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
    }

    #[tokio::test]
    async fn malformed_whitelist_is_never_overwritten() {
        let ctx = setup("malformed").await;
        write_props(&ctx, "online-mode=false\n");
        let raw = "{ this is not json";
        std::fs::write(ctx.rt.dir.join("whitelist.json"), raw).unwrap();
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Error, "{out:?}");
        assert_eq!(std::fs::read_to_string(ctx.rt.dir.join("whitelist.json")).unwrap(), raw);
        assert_eq!(read_state(&ctx.rt).await.status, "error");
    }

    #[tokio::test]
    async fn panel_managed_entry_is_removed_on_revoke() {
        let ctx = setup("removal").await;
        write_props(&ctx, "online-mode=false\n");
        reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
        let out = reconcile_instance(&ctx.state, &ctx.rt, &[], 2).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(out.removed, vec!["Steve".to_string()]);
        assert!(whitelist_names(&ctx.rt).is_empty());
    }

    #[tokio::test]
    async fn rename_removes_old_before_pending_new() {
        let _g = ONLINE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let ctx = setup("rename").await;
        write_props(&ctx, "online-mode=true\n");
        set_online_override(Some(vec![("Old", "uuid-old")]));
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Old")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");

        set_online_override(Some(vec![]));
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "New")]), 2).await;
        set_online_override(None);
        assert_eq!(out.status, SyncStatus::Partial, "{out:?}");
        assert_eq!(out.removed, vec!["Old".to_string()]);
        assert!(out.pending.iter().any(|p| p.name == "New"));
        assert!(whitelist_names(&ctx.rt).is_empty());
        assert!(!read_state(&ctx.rt).await.managed.contains_key("old"));
    }

    #[tokio::test]
    async fn stale_generation_is_skipped() {
        let ctx = setup("stale").await;
        write_props(&ctx, "online-mode=false\n");
        reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 5).await;
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Alex")]), 3).await;
        assert_eq!(out.status, SyncStatus::Skipped, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
        assert_eq!(read_state(&ctx.rt).await.desired[0].name, "Steve");
    }

    #[tokio::test]
    async fn explicit_reconcile_converges_after_transient_failure() {
        let _g = ONLINE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let ctx = setup("retry").await;
        write_props(&ctx, "online-mode=true\n");
        set_online_override(Some(vec![]));
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Pending, "{out:?}");
        assert!(!ctx.rt.dir.join("whitelist.json").exists());

        set_online_override(Some(vec![("Steve", "uuid-steve")]));
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 2).await;
        set_online_override(None);
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
    }

    #[tokio::test]
    async fn running_instance_without_command_channel_reports_pending() {
        let ctx = setup("running").await;
        write_props(&ctx, "online-mode=false\n");
        *ctx.rt.status.lock().await = Status::Running;
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Pending, "{out:?}");
        assert!(out.error.as_deref().unwrap_or("").contains("whitelist add"));
        assert!(!ctx.rt.dir.join("whitelist.json").exists());
    }

    #[tokio::test]
    async fn proxy_instance_is_unsupported() {
        let ctx = setup("proxy").await;
        ctx.rt.meta.write().await.jar = Some("velocity-3.3.0.jar".into());
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Unsupported, "{out:?}");
        assert!(!ctx.rt.dir.join("whitelist.json").exists());
    }

    #[tokio::test]
    async fn bungee_forwarding_requires_override_then_applies() {
        let ctx = setup("bungee").await;
        write_props(&ctx, "online-mode=false\nbungeecord=true\n");
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::NeedsOverride, "{out:?}");
        ctx.rt.meta.write().await.whitelist_identity = Some(WhitelistIdentity::Offline);
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 2).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
    }

    #[tokio::test]
    async fn verify_convergence_detects_and_times_out() {
        let ctx = setup("verify").await;
        std::fs::write(
            ctx.rt.dir.join("whitelist.json"),
            r#"[{"uuid":"x","name":"Steve"}]"#,
        )
        .unwrap();
        verify_convergence(&ctx.rt.dir, &["Steve".into()], &[], Duration::from_millis(300))
            .await
            .unwrap();
        verify_convergence(&ctx.rt.dir, &[], &["Alex".into()], Duration::from_millis(300))
            .await
            .unwrap();
        let err = verify_convergence(&ctx.rt.dir, &["Alex".into()], &[], Duration::from_millis(150)).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn concurrent_duplicate_updates_converge_without_temp_leftovers() {
        let ctx = setup("concurrent").await;
        write_props(&ctx, "online-mode=false\n");
        let desired = members(&[("u1", "Steve"), ("u2", "Alex")]);
        let mut handles = Vec::new();
        for _ in 0..2 {
            let state = ctx.state.clone();
            let rt = ctx.rt.clone();
            let d = desired.clone();
            handles.push(tokio::spawn(async move {
                reconcile_instance(&state, &rt, &d, 1).await
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(whitelist_names(&ctx.rt), vec!["Alex".to_string(), "Steve".to_string()]);
        let arr = users::read_array_strict(&ctx.rt.dir, "whitelist.json").unwrap();
        assert_eq!(arr.len(), 2, "不得产生重复条目");
        let leftovers: Vec<String> = std::fs::read_dir(&ctx.rt.dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
    }

    #[tokio::test]
    async fn mark_manual_retained_protects_existing_panel_entry() {
        let ctx = setup("mark").await;
        write_props(&ctx, "online-mode=false\n");
        reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        mark_manual_retained(&ctx.rt, "Steve").await.unwrap();
        let out = reconcile_instance(&ctx.state, &ctx.rt, &[], 2).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
    }

    #[tokio::test]
    async fn sync_all_reconciles_all_instances() {
        let ctx = setup("syncall").await;
        write_props(&ctx, "online-mode=false\n");
        let dir2 = ctx.rt.dir.parent().unwrap().join("inst2");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("server.properties"), "online-mode=false\n").unwrap();
        let meta2 = InstanceMeta {
            id: "i2".into(),
            name: "inst2".into(),
            ..Default::default()
        };
        let rt2 = InstanceRuntime::new(meta2, dir2.clone());
        ctx.state.instances.write().await.insert("i2".into(), rt2);
        ctx.state
            .instances
            .write()
            .await
            .insert("i1".into(), ctx.rt.clone());

        let snap = DesiredSnapshot::from_grants(
            7,
            vec![
                ("a".into(), "Admin".into(), vec![], true),
                ("b".into(), "Bob".into(), vec!["i1".into()], false),
            ],
        );
        let report = sync_all(&ctx.state, &snap).await;
        assert!(report.ok, "{report:?}");
        assert_eq!(report.revision, 7);
        assert_eq!(whitelist_names(&ctx.rt), vec!["Admin".to_string(), "Bob".to_string()]);
        let arr2 = users::read_array_strict(&dir2, "whitelist.json").unwrap();
        let mut n2: Vec<String> = arr2.iter().filter_map(entry_name).map(|s| s.to_string()).collect();
        n2.sort();
        assert_eq!(n2, vec!["Admin".to_string()]);
    }

    #[test]
    fn effective_members_scope_admin_and_assignments() {
        let snap = DesiredSnapshot {
            revision: 1,
            grants: vec![
                DesiredGrant { user_id: "a".into(), name: "Admin".into(), instance_ids: vec![], is_admin: true },
                DesiredGrant { user_id: "b".into(), name: "Bob".into(), instance_ids: vec!["i1".into()], is_admin: false },
                DesiredGrant { user_id: "c".into(), name: "Cara".into(), instance_ids: vec!["i2".into()], is_admin: false },
                DesiredGrant { user_id: "d".into(), name: "".into(), instance_ids: vec!["i1".into()], is_admin: false },
            ],
        };
        let i1: Vec<String> = effective_members(&snap, "i1").into_iter().map(|m| m.name).collect();
        assert_eq!(i1, vec!["Admin".to_string(), "Bob".to_string()]);
        let i2: Vec<String> = effective_members(&snap, "i2").into_iter().map(|m| m.name).collect();
        assert_eq!(i2, vec!["Admin".to_string(), "Cara".to_string()]);
    }
    #[tokio::test]
    async fn newer_pending_generation_not_overwritten_by_older() {
        let ctx = setup("stale-pending").await;
        // 代理实例：身份不支持，代号被持久化但 applied_generation 不变
        ctx.rt.meta.write().await.jar = Some("velocity-3.3.0.jar".into());
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "New")]), 10).await;
        assert_eq!(out.status, SyncStatus::Unsupported, "{out:?}");
        assert_eq!(read_state(&ctx.rt).await.generation, 10);

        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Old")]), 7).await;
        assert_eq!(out.status, SyncStatus::Skipped, "{out:?}");
        assert_eq!(read_state(&ctx.rt).await.desired[0].name, "New");
    }

    #[tokio::test]
    async fn unconfirmed_removal_keeps_panel_ownership_for_retry() {
        let ctx = setup("retry-removal").await;
        write_props(&ctx, "online-mode=false\n");
        reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);

        // 运行中但无命令通道：移除失败，所有权必须保留以便重试
        *ctx.rt.status.lock().await = Status::Running;
        let out = reconcile_instance(&ctx.state, &ctx.rt, &[], 2).await;
        assert_eq!(out.status, SyncStatus::Pending, "{out:?}");
        let st = read_state(&ctx.rt).await;
        assert_eq!(st.managed.get("steve").map(|e| e.origin.as_str()), Some("panel"));
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
    }

    #[tokio::test]
    async fn corrupt_sidecar_blocks_whitelist_mutation() {
        let ctx = setup("corrupt-sidecar").await;
        write_props(&ctx, "online-mode=false\n");
        std::fs::write(ctx.rt.dir.join("whitelist-sync.json"), "{ not json").unwrap();
        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Error, "{out:?}");
        assert!(!ctx.rt.dir.join("whitelist.json").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn whitelist_symlink_target_is_replaced_not_followed() {
        let ctx = setup("symlink-whitelist").await;
        write_props(&ctx, "online-mode=false\n");
        let outside = ctx.root.join("outside.json");
        std::fs::write(&outside, "[]").unwrap();
        std::os::unix::fs::symlink(&outside, ctx.rt.dir.join("whitelist.json")).unwrap();

        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        // 实例目录外的文件未被写入，白名单链接被替换为普通文件
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "[]");
        let md = std::fs::symlink_metadata(ctx.rt.dir.join("whitelist.json")).unwrap();
        assert!(md.file_type().is_file());
        assert_eq!(whitelist_names(&ctx.rt), vec!["Steve".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sidecar_symlink_target_is_replaced_not_followed() {
        let ctx = setup("symlink-sidecar").await;
        write_props(&ctx, "online-mode=false\n");
        let outside = ctx.root.join("outside-sidecar.json");
        std::fs::write(&outside, r#"{"schema_version":1}"#).unwrap();
        std::os::unix::fs::symlink(&outside, ctx.rt.dir.join("whitelist-sync.json")).unwrap();

        let out = reconcile_instance(&ctx.state, &ctx.rt, &members(&[("u1", "Steve")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), r#"{"schema_version":1}"#);
        let md = std::fs::symlink_metadata(ctx.rt.dir.join("whitelist-sync.json")).unwrap();
        assert!(md.file_type().is_file());
    }

    #[tokio::test]
    async fn cloned_instance_does_not_inherit_source_grants() {
        let ctx = setup("clone").await;
        write_props(&ctx, "online-mode=false\n");
        // 源实例：管理员 + 普通用户（均由面板管理）
        let src = members(&[("u-admin", "Admin"), ("u-ordinary", "Steve")]);
        let out = reconcile_instance(&ctx.state, &ctx.rt, &src, 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(whitelist_names(&ctx.rt), vec!["Admin".to_string(), "Steve".to_string()]);

        // 模拟克隆：复制源实例文件（含白名单与旁路状态）到新实例目录
        let clone_dir = ctx.root.join("inst-clone");
        std::fs::create_dir_all(&clone_dir).unwrap();
        for f in ["server.properties", "whitelist.json", "whitelist-sync.json"] {
            std::fs::copy(ctx.rt.dir.join(f), clone_dir.join(f)).unwrap();
        }
        let clone_meta = InstanceMeta {
            id: "clone".into(),
            name: "clone".into(),
            ..Default::default()
        };
        std::fs::write(clone_dir.join("instance.json"), serde_json::to_string(&clone_meta).unwrap())
            .unwrap();
        let clone_rt = InstanceRuntime::new(clone_meta, clone_dir);

        // 新实例的权威授权仅管理员：普通用户授权不会随克隆继承，面板管理条目被移除
        let out = reconcile_instance(&ctx.state, &clone_rt, &members(&[("u-admin", "Admin")]), 1).await;
        assert_eq!(out.status, SyncStatus::Applied, "{out:?}");
        assert_eq!(out.removed, vec!["Steve".to_string()]);
        assert_eq!(whitelist_names(&clone_rt), vec!["Admin".to_string()]);
        // 源实例不受影响
        assert_eq!(whitelist_names(&ctx.rt), vec!["Admin".to_string(), "Steve".to_string()]);
    }
}
