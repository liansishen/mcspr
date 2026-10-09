//! 账户、密码与会话认证。
//!
//! 账户保存在 `data_dir/auth/users.json`，会话仅存内存；面板重启后需重新登录。
//! 浏览器使用 HttpOnly Cookie；密码、角色或启用状态变化会撤销账户旧会话。
//! 存储持有 `data_dir/auth/.lock` 的排他文件锁，本地账户命令要求面板服务已停止。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use fs4::fs_std::FileExt;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, Semaphore};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// 账户文件 schema 版本
const SCHEMA_VERSION: u32 = 2;
/// 存储排他文件锁
const LOCK_FILE: &str = ".lock";
/// 会话有效期（24 小时）
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_USERNAME_LEN: usize = 32;
const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 128;
/// 申请理由最大长度
const MAX_REASON_LEN: usize = 1000;
/// Minecraft 游戏名长度范围（保留原始大小写，唯一性忽略大小写）
const MIN_MC_NAME_LEN: usize = 3;
const MAX_MC_NAME_LEN: usize = 16;
/// 单个账户保留的游戏名变更审核历史上限
const MAX_NAME_HISTORY: usize = 20;
/// 同时进行的 Argon2 哈希/校验任务上限，避免登录风暴耗尽 CPU
const MAX_HASH_CONCURRENCY: usize = 4;
/// 登录失败限流窗口与阈值（账户维度 / 来源维度）
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RATE_MAX_PER_ACCOUNT: usize = 10;
const RATE_MAX_PER_SOURCE: usize = 30;
/// 限流表键数量上限：达到上限后不再登记新键，但保留既有键的限流（不清空活动限制）
const RATE_MAX_KEYS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

/// 账户注册审批状态；`enabled` 独立表示管理员禁用，登录须同时满足已批准且启用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AccountStatus {
    Pending,
    #[default]
    Approved,
    Rejected,
}

impl AccountStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountStatus::Pending => "pending",
            AccountStatus::Approved => "approved",
            AccountStatus::Rejected => "rejected",
        }
    }
}

/// 游戏名变更申请（保留原始大小写；等待或拒绝时旧名继续生效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NameChangeRequest {
    pub id: String,
    pub new_name: String,
    #[serde(default)]
    pub reason: String,
    pub status: AccountStatus,
    pub revision: u64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub reviewed_at: Option<String>,
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub rejection_reason: Option<String>,
}

/// 已批准改名后保留的旧游戏名：相关实例确认移除前禁止复用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetiredName {
    /// 旧游戏名（保留原始大小写）
    pub name: String,
    /// 改名时该账户的显式实例授权；管理员为空但 `global=true`
    #[serde(default)]
    pub instance_ids: Vec<String>,
    /// 改名时是否具有全实例授权
    #[serde(default)]
    pub global: bool,
    #[serde(default)]
    pub retired_at: String,
}

/// 协调器对旧游戏名在全部实例上的移除判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetiredNameStatus {
    /// 已确认该实例不再保留旧名，可释放
    Removed,
    /// 该实例仍保留旧名（面板管理或人工保留）
    Retained,
    /// 无法确认（实例缺失 / 同步未完成），保守保留
    Unknown,
}

/// 对外展示的申请（注册申请或游戏名变更申请）。
#[derive(Debug, Clone, Serialize)]
pub struct Application {
    pub id: String,
    /// registration / name_change
    pub kind: String,
    pub user_id: String,
    pub username: String,
    pub minecraft_name: Option<String>,
    pub reason: String,
    pub status: AccountStatus,
    pub revision: u64,
    pub rejection_reason: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub enabled: bool,
    #[serde(default)]
    pub instance_ids: Vec<String>,
    /// 会话纪元：密码 / 角色 / 启用状态变化时自增，使旧会话立即失效
    #[serde(default)]
    pub session_epoch: u64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// 游戏名（保留原始大小写；None 表示尚未绑定）
    #[serde(default)]
    pub minecraft_name: Option<String>,
    /// 注册审批状态
    #[serde(default)]
    pub status: AccountStatus,
    /// 注册申请理由
    #[serde(default)]
    pub application_reason: String,
    /// 申请修订号：重申时自增，审核须匹配以避免批准旧资料
    #[serde(default)]
    pub application_revision: u64,
    #[serde(default)]
    pub reviewed_at: Option<String>,
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub rejection_reason: Option<String>,
    /// 待处理的游戏名变更申请
    #[serde(default)]
    pub pending_name_change: Option<NameChangeRequest>,
    /// 有限的游戏名变更审核历史
    #[serde(default)]
    pub name_change_history: Vec<NameChangeRequest>,
    /// 改名后保留的旧游戏名：跨实例白名单移除确认前不得复用
    #[serde(default)]
    pub retired_names: Vec<RetiredName>,
}

/// 对外返回的账户字段（不含密码哈希等内部数据）
#[derive(Debug, Clone, Serialize)]
pub struct PublicUser {
    pub id: String,
    pub username: String,
    pub role: Role,
    pub enabled: bool,
    pub instance_ids: Vec<String>,
    /// 游戏名（不含申请理由等敏感信息）
    pub minecraft_name: Option<String>,
    pub status: AccountStatus,
}

impl From<&User> for PublicUser {
    fn from(u: &User) -> Self {
        Self {
            id: u.id.clone(),
            username: u.username.clone(),
            role: u.role,
            enabled: u.enabled,
            instance_ids: u.instance_ids.clone(),
            minecraft_name: u.minecraft_name.clone(),
            status: u.status,
        }
    }
}

/// 请求级身份，由中间件从当前会话解析并放入请求扩展；每次请求都重新读取最新账户状态。
#[derive(Debug, Clone)]
pub struct Identity {
    pub user_id: String,
    pub username: String,
    pub role: Role,
    pub instance_ids: Vec<String>,
    pub csrf: String,
    /// 会话摘要（不落盘、不外泄），用于 WebSocket 周期校验与撤销
    pub session: String,
}

impl Identity {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    /// 是否可查看指定实例：管理员全部可见，普通用户仅限授权实例
    pub fn can_view(&self, instance_id: &str) -> bool {
        self.is_admin() || self.instance_ids.iter().any(|i| i == instance_id)
    }
}

/// 具名授权快照：`revision` 与 `grants` 在同一读锁内采集，二者严格对应。
#[derive(Debug, Clone)]
pub struct NamedGrantsSnapshot {
    pub revision: u64,
    pub grants: Vec<(String, String, Vec<String>, bool)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AccountFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// 全局授权代际，与 users 同事务持久化；重启后恢复，避免白名单同步代号被误判陈旧。
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    users: Vec<User>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// 校验账户文件 schema：拒绝 0 与高于当前支持版本的记录，避免误读未来格式。
fn validate_schema(version: u32) -> Result<(), String> {
    if version == 0 || version > SCHEMA_VERSION {
        return Err(format!(
            "不支持的账户文件 schema_version={version}（本面板支持 1..={SCHEMA_VERSION}）"
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct Session {
    user_id: String,
    csrf: String,
    epoch: u64,
    expires_at: Instant,
}

#[derive(Default)]
struct RateLimiter {
    hits: HashMap<String, VecDeque<Instant>>,
}

impl RateLimiter {
    fn trim_at(&mut self, key: &str, now: Instant) {
        if let Some(q) = self.hits.get_mut(key) {
            while q
                .front()
                .map(|t| now.duration_since(*t) > RATE_WINDOW)
                .unwrap_or(false)
            {
                q.pop_front();
            }
            if q.is_empty() {
                self.hits.remove(key);
            }
        }
    }

    /// 原子地检查并计入一次尝试：先清理过期命中，再判断额度，最后计数。
    /// 达到键上限且需要登记新键时，先全局清理过期键尝试恢复；仍无空间则失败关闭（拒绝新键）。
    fn reserve_at(&mut self, keys: &[(&str, usize)], now: Instant) -> bool {
        for (k, _) in keys {
            self.trim_at(k, now);
        }
        for (k, max) in keys {
            if self.hits.get(*k).map(|q| q.len() >= *max).unwrap_or(false) {
                return false;
            }
        }
        let needs_new_key = keys.iter().any(|(k, _)| !self.hits.contains_key(*k));
        if needs_new_key && self.hits.len() >= RATE_MAX_KEYS {
            self.purge_expired(now);
            if self.hits.len() >= RATE_MAX_KEYS {
                return false;
            }
        }
        for (k, _) in keys {
            match self.hits.get_mut(*k) {
                Some(q) => q.push_back(now),
                None => {
                    let mut q = VecDeque::new();
                    q.push_back(now);
                    self.hits.insert((*k).to_string(), q);
                }
            }
        }
        true
    }

    /// 全局清理过期命中，释放键位以便限流表恢复；窗口内的活动限制保持不变。
    fn purge_expired(&mut self, now: Instant) {
        self.hits.retain(|_, q| {
            while q
                .front()
                .map(|t| now.duration_since(*t) > RATE_WINDOW)
                .unwrap_or(false)
            {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    fn clear(&mut self, key: &str) {
        self.hits.remove(key);
    }
}

struct Inner {
    path: PathBuf,
    /// 保持文件句柄打开，持有存储排他锁
    _lock: std::fs::File,
    users: RwLock<Vec<User>>,
    sessions: Mutex<HashMap<String, Session>>,
    limiter: Mutex<RateLimiter>,
    hashing: Arc<Semaphore>,
    /// 全局授权代际：任何账户/授权/游戏名变更都会自增，供白名单同步轮询
    revision: AtomicU64,
}

/// 账户与会话存储，克隆共享同一份内存状态。
#[derive(Clone)]
pub struct AuthStore {
    inner: Arc<Inner>,
}

impl AuthStore {
    /// 从数据目录加载账户。
    ///
    /// 会先获取 `data_dir/auth/.lock` 的生命周期排他锁：同一数据目录若已有活动的服务或 CLI
    /// 存储，则直接返回错误。文件不存在视为空存储；解析失败或 schema 不受支持返回错误并保持
    /// 访问封闭。
    pub fn load(data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = data_dir.as_ref().join("auth");
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("创建账户目录失败 {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        let lock_path = dir.join(LOCK_FILE);
        let mut lock_opts = std::fs::OpenOptions::new();
        lock_opts.create(true).read(true).write(true);
        #[cfg(unix)]
        lock_opts.mode(0o600);
        let lock = lock_opts
            .open(&lock_path)
            .map_err(|e| anyhow::anyhow!("打开账户锁文件失败 {}: {e}", lock_path.display()))?;
        lock.try_lock_exclusive().map_err(|_| {
            anyhow::anyhow!(
                "账户存储正被其他进程使用（{}）；请先停止面板服务后再执行本地管理命令",
                lock_path.display()
            )
        })?;

        let path = dir.join("users.json");
        let (users, revision) = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("读取账户文件失败 {}: {e}", path.display()))?;
            let parsed: AccountFile = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("账户文件解析失败 {}: {e}", path.display()))?;
            validate_schema(parsed.schema_version)
                .map_err(|e| anyhow::anyhow!("账户文件无效 {}: {e}", path.display()))?;
            // 旧 schema 无 revision 字段时 serde 默认 0，仍可加载
            (parsed.users, parsed.revision)
        } else {
            (Vec::new(), 0)
        };

        Ok(Self {
            inner: Arc::new(Inner {
                path,
                _lock: lock,
                users: RwLock::new(users),
                sessions: Mutex::new(HashMap::new()),
                limiter: Mutex::new(RateLimiter::default()),
                hashing: Arc::new(Semaphore::new(MAX_HASH_CONCURRENCY)),
                revision: AtomicU64::new(revision),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    // ---------- 账户查询 ----------

    pub async fn has_users(&self) -> bool {
        !self.inner.users.read().await.is_empty()
    }

    pub async fn list(&self) -> Vec<PublicUser> {
        let mut users: Vec<PublicUser> = self
            .inner
            .users
            .read()
            .await
            .iter()
            .map(PublicUser::from)
            .collect();
        users.sort_by(|a, b| a.username.cmp(&b.username));
        users
    }

    pub async fn get(&self, id: &str) -> Option<PublicUser> {
        self.inner
            .users
            .read()
            .await
            .iter()
            .find(|u| u.id == id)
            .map(PublicUser::from)
    }

    pub async fn find_by_username(&self, username: &str) -> Option<PublicUser> {
        let username = normalize_username(username);
        self.inner
            .users
            .read()
            .await
            .iter()
            .find(|u| u.username == username)
            .map(PublicUser::from)
    }

    // ---------- 账户变更 ----------

    pub async fn create_user(
        &self,
        username: &str,
        password: &str,
        role: Role,
        instance_ids: Vec<String>,
    ) -> Result<PublicUser, String> {
        let username = normalize_username(username);
        validate_username(&username)?;
        validate_password(password)?;
        // 哈希在持有写锁之前完成，避免 Argon2 阻塞其他账户操作
        let hash = self.hash_password(password).await?;
        let now = crate::util::now_str();
        let user = User {
            id: uuid::Uuid::new_v4().to_string(),
            username,
            password_hash: hash,
            role,
            enabled: true,
            instance_ids: dedup_ids(instance_ids),
            session_epoch: 0,
            created_at: now.clone(),
            updated_at: now,
            minecraft_name: None,
            status: AccountStatus::Approved,
            application_reason: String::new(),
            application_revision: 0,
            reviewed_at: None,
            reviewer: None,
            rejection_reason: None,
            pending_name_change: None,
            name_change_history: Vec::new(),
            retired_names: Vec::new(),
        };
        let created = user.clone();
        self.mutate(|users| {
            if users.iter().any(|u| u.username == created.username) {
                return Err("用户名已存在".to_string());
            }
            users.push(created.clone());
            Ok(())
        })
        .await?;
        Ok(PublicUser::from(&user))
    }

    /// 校验用户名 / 密码，返回账户快照与其凭据纪元。
    ///
    /// 调用方须把该纪元传给 [`AuthStore::start_session`]，以拒绝“校验后又发生重置”的竞态。
    pub async fn verify_credentials(
        &self,
        username: &str,
        password: &str,
    ) -> Option<(PublicUser, u64)> {
        let username = normalize_username(username);
        let user = self
            .inner
            .users
            .read()
            .await
            .iter()
            .find(|u| u.username == username)
            .cloned();
        // 未知账户也执行一次等价的 Argon2 校验，避免通过响应时间枚举账户。
        let hash = match &user {
            Some(u) => u.password_hash.clone(),
            None => dummy_password_hash().to_string(),
        };
        let ok = self.verify_password(&hash, password).await;
        match user {
            Some(u) if ok && u.enabled => Some((PublicUser::from(&u), u.session_epoch)),
            _ => None,
        }
    }

    /// 在当前凭据纪元仍与校验时一致的前提下创建会话，避免旧密码通过重置竞态拿到新会话。
    pub async fn start_session(
        &self,
        user_id: &str,
        expected_epoch: u64,
    ) -> Option<(String, String)> {
        let user = {
            self.inner
                .users
                .read()
                .await
                .iter()
                .find(|u| u.id == user_id)
                .cloned()
        }?;
        if !user.enabled
            || user.status != AccountStatus::Approved
            || user.session_epoch != expected_epoch
        {
            return None;
        }
        let token = random_token();
        let csrf = random_token();
        let digest = session_digest(&token);
        let now = Instant::now();
        let mut sessions = self.inner.sessions.lock().await;
        sessions.retain(|_, s| s.expires_at > now);
        sessions.insert(
            digest,
            Session {
                user_id: user.id,
                csrf: csrf.clone(),
                epoch: expected_epoch,
                expires_at: now + SESSION_TTL,
            },
        );
        Some((token, csrf))
    }

    pub async fn change_password(
        &self,
        user_id: &str,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), String> {
        validate_password(new_password)?;
        let (hash, epoch) = {
            self.inner
                .users
                .read()
                .await
                .iter()
                .find(|u| u.id == user_id)
                .map(|u| (u.password_hash.clone(), u.session_epoch))
        }
        .ok_or_else(|| "账户不存在".to_string())?;
        if !self.verify_password(&hash, old_password).await {
            return Err("原密码不正确".to_string());
        }
        let new_hash = self.hash_password(new_password).await?;
        // 写入前复核原哈希与纪元：若期间发生重置，拒绝覆盖
        self.apply_password_change(user_id, hash, epoch, new_hash)
            .await
    }

    async fn apply_password_change(
        &self,
        user_id: &str,
        expected_hash: String,
        expected_epoch: u64,
        new_hash: String,
    ) -> Result<(), String> {
        self.mutate(|users| {
            let u = users
                .iter_mut()
                .find(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            if u.password_hash != expected_hash || u.session_epoch != expected_epoch {
                return Err("账户凭据已变更，请重试".to_string());
            }
            u.password_hash = new_hash;
            u.session_epoch += 1;
            u.updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    pub async fn reset_password(&self, user_id: &str, new_password: &str) -> Result<(), String> {
        validate_password(new_password)?;
        let new_hash = self.hash_password(new_password).await?;
        self.mutate(|users| {
            let u = users
                .iter_mut()
                .find(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            u.password_hash = new_hash;
            u.session_epoch += 1;
            u.updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    /// 原子更新启用状态与角色，并在同一次写入中校验“至少保留一个可用管理员”。
    pub async fn update_user(
        &self,
        user_id: &str,
        enabled: Option<bool>,
        role: Option<Role>,
    ) -> Result<PublicUser, String> {
        self.mutate(|users| {
            let idx = users
                .iter()
                .position(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            let old_role = users[idx].role;
            let old_enabled = users[idx].enabled;
            if let Some(r) = role {
                users[idx].role = r;
            }
            if let Some(e) = enabled {
                users[idx].enabled = e;
            }
            if users
                .iter()
                .filter(|u| u.role == Role::Admin && u.enabled)
                .count()
                == 0
            {
                return Err("不能移除最后一个可用管理员".to_string());
            }
            if users[idx].role != old_role || users[idx].enabled != old_enabled {
                users[idx].session_epoch += 1;
                users[idx].updated_at = crate::util::now_str();
            }
            Ok(PublicUser::from(&users[idx]))
        })
        .await
    }

    #[cfg(test)]
    pub async fn set_enabled(&self, user_id: &str, enabled: bool) -> Result<(), String> {
        self.update_user(user_id, Some(enabled), None)
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub async fn set_role(&self, user_id: &str, role: Role) -> Result<(), String> {
        self.update_user(user_id, None, Some(role))
            .await
            .map(|_| ())
    }

    /// 全量保存授权实例；单次原子写入，授权变更在下次请求即生效，无需撤销会话。
    pub async fn set_instances(
        &self,
        user_id: &str,
        instance_ids: Vec<String>,
    ) -> Result<(), String> {
        let ids = dedup_ids(instance_ids);
        self.mutate(|users| {
            let u = users
                .iter_mut()
                .find(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            u.instance_ids = ids.clone();
            u.updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    // ---------- 注册审批 / 游戏名变更 ----------

    /// 全局授权代际：任何账户、授权或游戏名变更后自增，供白名单同步轮询。
    pub fn revision(&self) -> u64 {
        self.inner.revision.load(Ordering::SeqCst)
    }

    /// 已批准且启用的具名账户授权快照，供白名单同步使用。
    ///
    /// 返回 `(user_id, minecraft_name, instance_ids, is_admin)`；管理员 `is_admin=true`，
    /// 其有效范围为全部实例，`instance_ids` 仅记录显式授权。缺少游戏名的账户不生成条目。
    pub async fn approved_named_grants(&self) -> Vec<(String, String, Vec<String>, bool)> {
        self.approved_named_grants_with_revision().await.grants
    }

    pub async fn approved_grants_snapshot(&self) -> (u64, Vec<(String, String, Vec<String>, bool)>) {
        let snapshot = self.approved_named_grants_with_revision().await;
        (snapshot.revision, snapshot.grants)
    }

    /// 在同一账户读锁内读取具名授权与持久化修订号。
    pub async fn approved_named_grants_with_revision(&self) -> NamedGrantsSnapshot {
        let users = self.inner.users.read().await;
        let grants = named_grants(&users);
        let revision = self.inner.revision.load(Ordering::SeqCst);
        NamedGrantsSnapshot { revision, grants }
    }

    /// 注册：始终创建待审批的普通用户，忽略客户端可注入的角色与授权。
    pub async fn register_pending_user(
        &self,
        username: &str,
        password: &str,
        minecraft_name: &str,
        reason: &str,
    ) -> Result<PublicUser, String> {
        let username = normalize_username(username);
        validate_username(&username)?;
        validate_password(password)?;
        let mc_name = normalize_minecraft_name(minecraft_name)?;
        let reason = reason.trim().to_string();
        if reason.chars().count() > MAX_REASON_LEN {
            return Err(format!("申请理由最长 {MAX_REASON_LEN} 个字符"));
        }
        let hash = self.hash_password(password).await?;
        let now = crate::util::now_str();
        let user = User {
            id: uuid::Uuid::new_v4().to_string(),
            username,
            password_hash: hash,
            role: Role::User,
            enabled: true,
            instance_ids: Vec::new(),
            session_epoch: 0,
            created_at: now.clone(),
            updated_at: now,
            minecraft_name: Some(mc_name.clone()),
            status: AccountStatus::Pending,
            application_reason: reason,
            application_revision: 1,
            reviewed_at: None,
            reviewer: None,
            rejection_reason: None,
            pending_name_change: None,
            name_change_history: Vec::new(),
            retired_names: Vec::new(),
        };
        let created = user.clone();
        self.mutate(|users| {
            if !users
                .iter()
                .any(|u| u.role == Role::Admin && u.enabled && u.status == AccountStatus::Approved)
            {
                return Err("面板尚未完成初始化，请先创建管理员".to_string());
            }
            if users.iter().any(|u| u.username == created.username) {
                return Err("用户名已存在".to_string());
            }
            if name_taken(users, &mc_name, None) {
                return Err("该游戏名已被占用".to_string());
            }
            users.push(created.clone());
            Ok(())
        })
        .await?;
        Ok(PublicUser::from(&user))
    }

    /// 被拒绝的注册申请重申：更新资料并回到待审批，修订号自增使旧审核失效。
    pub async fn resubmit_application(
        &self,
        user_id: &str,
        minecraft_name: &str,
        reason: &str,
    ) -> Result<(), String> {
        let mc_name = normalize_minecraft_name(minecraft_name)?;
        let reason = reason.trim().to_string();
        if reason.chars().count() > MAX_REASON_LEN {
            return Err(format!("申请理由最长 {MAX_REASON_LEN} 个字符"));
        }
        self.mutate(|users| {
            let idx = users
                .iter()
                .position(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            if users[idx].status != AccountStatus::Rejected {
                return Err("仅被拒绝的申请可以重新提交".to_string());
            }
            if name_taken(users, &mc_name, Some(user_id)) {
                return Err("该游戏名已被占用".to_string());
            }
            let u = &mut users[idx];
            u.minecraft_name = Some(mc_name.clone());
            u.application_reason = reason.clone();
            u.status = AccountStatus::Pending;
            u.application_revision += 1;
            u.reviewer = None;
            u.rejection_reason = None;
            u.reviewed_at = None;
            u.updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    /// 管理员批准申请（注册申请或游戏名变更申请），修订号必须匹配。
    ///
    /// 注册批准时在同一次原子写入中分配实例授权。
    pub async fn approve_application(
        &self,
        id: &str,
        expected_revision: u64,
        instance_ids: Vec<String>,
        reviewer: &str,
    ) -> Result<(), String> {
        let ids = dedup_ids(instance_ids);
        let reviewer = reviewer.to_string();
        self.mutate(|users| {
            if let Some(idx) = users.iter().position(|u| u.id == id) {
                if users[idx].status != AccountStatus::Pending {
                    return Err("申请状态已变化".to_string());
                }
                if users[idx].application_revision != expected_revision {
                    return Err("申请已被修改，请刷新后重试".to_string());
                }
                let u = &mut users[idx];
                u.status = AccountStatus::Approved;
                u.instance_ids = ids.clone();
                u.reviewer = Some(reviewer.clone());
                u.reviewed_at = Some(crate::util::now_str());
                u.rejection_reason = None;
                u.application_revision += 1;
                u.updated_at = crate::util::now_str();
                return Ok(());
            }
            let pos = users
                .iter()
                .position(|u| {
                    u.pending_name_change
                        .as_ref()
                        .map(|r| r.id == id)
                        .unwrap_or(false)
                })
                .ok_or("申请不存在")?;
            let (rev, new_name) = {
                let r = users[pos].pending_name_change.as_ref().unwrap();
                (r.revision, r.new_name.clone())
            };
            if rev != expected_revision {
                return Err("申请已被修改，请刷新后重试".to_string());
            }
            let owner = users[pos].id.clone();
            if name_taken(users, &new_name, Some(&owner)) {
                return Err("该游戏名已被占用".to_string());
            }
            let mut req = users[pos].pending_name_change.take().unwrap();
            req.status = AccountStatus::Approved;
            req.reviewer = Some(reviewer.clone());
            req.reviewed_at = Some(crate::util::now_str());
            req.rejection_reason = None;
            push_name_history(&mut users[pos], req);
            // 旧名进入退休保留：跨实例白名单移除确认前不得复用
            if let Some(old) = users[pos].minecraft_name.clone() {
                if !old.eq_ignore_ascii_case(new_name.trim()) {
                    let global = users[pos].role == Role::Admin;
                    let instance_ids = users[pos].instance_ids.clone();
                    users[pos].retired_names.push(RetiredName {
                        name: old,
                        instance_ids,
                        global,
                        retired_at: crate::util::now_str(),
                    });
                }
            }
            users[pos].minecraft_name = Some(new_name);
            users[pos].updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    /// 管理员拒绝申请；拒绝必须给出理由，旧游戏名继续生效。
    pub async fn reject_application(
        &self,
        id: &str,
        expected_revision: u64,
        reason: &str,
        reviewer: &str,
    ) -> Result<(), String> {
        let reason = reason.trim().to_string();
        if reason.is_empty() {
            return Err("拒绝理由不能为空".to_string());
        }
        if reason.chars().count() > MAX_REASON_LEN {
            return Err(format!("拒绝理由最长 {MAX_REASON_LEN} 个字符"));
        }
        let reviewer = reviewer.to_string();
        self.mutate(|users| {
            if let Some(idx) = users.iter().position(|u| u.id == id) {
                if users[idx].status != AccountStatus::Pending {
                    return Err("申请状态已变化".to_string());
                }
                if users[idx].application_revision != expected_revision {
                    return Err("申请已被修改，请刷新后重试".to_string());
                }
                let u = &mut users[idx];
                u.status = AccountStatus::Rejected;
                u.rejection_reason = Some(reason.clone());
                u.reviewer = Some(reviewer.clone());
                u.reviewed_at = Some(crate::util::now_str());
                u.application_revision += 1;
                u.updated_at = crate::util::now_str();
                return Ok(());
            }
            let pos = users
                .iter()
                .position(|u| {
                    u.pending_name_change
                        .as_ref()
                        .map(|r| r.id == id)
                        .unwrap_or(false)
                })
                .ok_or("申请不存在")?;
            let rev = users[pos].pending_name_change.as_ref().unwrap().revision;
            if rev != expected_revision {
                return Err("申请已被修改，请刷新后重试".to_string());
            }
            let mut req = users[pos].pending_name_change.take().unwrap();
            req.status = AccountStatus::Rejected;
            req.rejection_reason = Some(reason.clone());
            req.reviewer = Some(reviewer.clone());
            req.reviewed_at = Some(crate::util::now_str());
            push_name_history(&mut users[pos], req);
            users[pos].updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    /// 已批准用户提交游戏名变更申请（首次绑定游戏名同样走此流程）。
    pub async fn request_name_change(
        &self,
        user_id: &str,
        minecraft_name: &str,
        reason: &str,
    ) -> Result<NameChangeRequest, String> {
        let new_name = normalize_minecraft_name(minecraft_name)?;
        let reason = reason.trim().to_string();
        if reason.chars().count() > MAX_REASON_LEN {
            return Err(format!("申请理由最长 {MAX_REASON_LEN} 个字符"));
        }
        let req = NameChangeRequest {
            id: uuid::Uuid::new_v4().to_string(),
            new_name: new_name.clone(),
            reason,
            status: AccountStatus::Pending,
            revision: 1,
            created_at: crate::util::now_str(),
            reviewed_at: None,
            reviewer: None,
            rejection_reason: None,
        };
        let stored = req.clone();
        self.mutate(|users| {
            let idx = users
                .iter()
                .position(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            if users[idx].status != AccountStatus::Approved {
                return Err("账户尚未通过审核".to_string());
            }
            if users[idx].pending_name_change.is_some() {
                return Err("已有待处理的游戏名变更申请".to_string());
            }
            if name_taken(users, &new_name, Some(user_id)) {
                return Err("该游戏名已被占用".to_string());
            }
            users[idx].pending_name_change = Some(stored.clone());
            users[idx].updated_at = crate::util::now_str();
            Ok(())
        })
        .await?;
        Ok(req)
    }

    /// 撤回自己的待处理游戏名变更申请。
    pub async fn withdraw_name_change(
        &self,
        user_id: &str,
        request_id: &str,
    ) -> Result<(), String> {
        self.mutate(|users| {
            let idx = users
                .iter()
                .position(|u| u.id == user_id)
                .ok_or("账户不存在")?;
            let matches = users[idx]
                .pending_name_change
                .as_ref()
                .map(|r| r.id == request_id)
                .unwrap_or(false);
            if !matches {
                return Err("申请不存在".to_string());
            }
            users[idx].pending_name_change = None;
            users[idx].updated_at = crate::util::now_str();
            Ok(())
        })
        .await
    }

    /// 全部待处理/被拒的注册申请与游戏名变更申请（管理员）。
    pub async fn applications(&self) -> Vec<Application> {
        let users = self.inner.users.read().await;
        let mut out = Vec::new();
        for u in users.iter() {
            if matches!(u.status, AccountStatus::Pending | AccountStatus::Rejected) {
                out.push(registration_application(u));
            }
            if let Some(r) = &u.pending_name_change {
                out.push(name_change_application(u, r));
            }
        }
        out
    }

    /// 指定用户当前的申请：注册申请（待审/被拒）或待处理的游戏名变更申请。
    pub async fn application_for_user(&self, user_id: &str) -> Option<Application> {
        let users = self.inner.users.read().await;
        let u = users.iter().find(|u| u.id == user_id)?;
        if matches!(u.status, AccountStatus::Pending | AccountStatus::Rejected) {
            return Some(registration_application(u));
        }
        u.pending_name_change
            .as_ref()
            .map(|r| name_change_application(u, r))
    }

    // ---------- 实例权限 ----------

    /// 权限视图快照：在同一读锁内取得账户列表与当前全局修订号，保证两者一致。
    pub async fn permission_snapshot(&self) -> (u64, Vec<PublicUser>) {
        let users = self.inner.users.read().await;
        let revision = self.revision();
        let mut list: Vec<PublicUser> = users.iter().map(PublicUser::from).collect();
        list.sort_by(|a, b| a.username.cmp(&b.username));
        (revision, list)
    }

    /// 实例权限修订号：统一采用全局授权代际。任何账户 / 授权变更都会使其自增，
    /// 因此创建、审批、直接授权等接口产生的权限变更同样会使旧权限快照失效，
    /// 避免旧快照无声回滚权限（无关写可能导致 409，但不会漏检）。
    pub async fn instance_permission_revision(&self, _instance_id: &str) -> u64 {
        self.revision()
    }

    /// 原子设置单个实例的授权账户集合：只改动该实例的授权，保留其余实例授权。
    ///
    /// `expected_revision` 为调用方读取到的全局修订号；与当前不一致则返回冲突错误，
    /// 避免用陈旧权限快照覆盖其他接口产生的授权变更。成功后返回新的全局修订号。
    pub async fn set_instance_grants_checked(
        &self,
        instance_id: &str,
        expected_revision: u64,
        user_ids: Vec<String>,
    ) -> Result<u64, String> {
        let wanted: HashSet<String> = user_ids
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let instance_id = instance_id.to_string();
        let ((), next) = self
            .mutate_revision_checked(Some(expected_revision), |users| {
                for id in &wanted {
                    if !users.iter().any(|u| &u.id == id) {
                        return Err(format!("账户不存在: {id}"));
                    }
                }
                for u in users.iter_mut() {
                    let has = u.instance_ids.iter().any(|i| i == &instance_id);
                    let want = wanted.contains(&u.id);
                    if want && !has {
                        u.instance_ids.push(instance_id.clone());
                        u.updated_at = crate::util::now_str();
                    } else if !want && has {
                        u.instance_ids.retain(|i| i != &instance_id);
                        u.updated_at = crate::util::now_str();
                    }
                }
                Ok(())
            })
            .await?;
        Ok(next)
    }

    /// 协调器释放已确认从相关实例移除的退休名；未确认的一律保守保留。
    /// `status_of` 按旧游戏名确认全部实例的移除结果，人工保留或无法确认时继续预留。
    pub async fn release_retired_names_if_removed<F>(&self, status_of: F) -> Result<usize, String>
    where
        F: Fn(&str) -> RetiredNameStatus,
    {
        // 先只读探测：无可释放项时不写盘、不推进修订号
        let any = {
            let users = self.inner.users.read().await;
            users.iter().any(|u| {
                u.retired_names
                    .iter()
                    .any(|r| retired_releasable(r, &status_of))
            })
        };
        if !any {
            return Ok(0);
        }
        let mut released = 0usize;
        self.mutate(|users| {
            for u in users.iter_mut() {
                let before = u.retired_names.len();
                u.retired_names
                    .retain(|r| !retired_releasable(r, &status_of));
                released += before - u.retired_names.len();
            }
            Ok(())
        })
        .await?;
        Ok(released)
    }

    pub async fn delete(&self, user_id: &str) -> Result<(), String> {
        self.mutate(|users| {
            let target = users.iter().find(|u| u.id == user_id).ok_or("账户不存在")?;
            if target.role == Role::Admin && target.enabled {
                let others = users
                    .iter()
                    .filter(|u| u.id != user_id && u.role == Role::Admin && u.enabled)
                    .count();
                if others == 0 {
                    return Err("不能删除最后一个可用管理员".to_string());
                }
            }
            users.retain(|u| u.id != user_id);
            Ok(())
        })
        .await?;
        let mut sessions = self.inner.sessions.lock().await;
        sessions.retain(|_, s| s.user_id != user_id);
        Ok(())
    }

    // ---------- 会话 ----------

    pub async fn session_identity(&self, token: &str) -> Option<Identity> {
        let digest = session_digest(token);
        self.identity_for_digest(&digest).await
    }

    /// 按会话摘要解析身份（WebSocket 周期校验使用）。
    pub async fn session_identity_by_digest(&self, digest: &str) -> Option<Identity> {
        self.identity_for_digest(digest).await
    }

    async fn identity_for_digest(&self, digest: &str) -> Option<Identity> {
        let session = { self.inner.sessions.lock().await.get(digest).cloned() }?;
        let expired = session.expires_at <= Instant::now();
        let user = if expired {
            None
        } else {
            self.inner
                .users
                .read()
                .await
                .iter()
                .find(|u| u.id == session.user_id)
                .cloned()
        };
        match user {
            Some(u)
                if u.enabled
                    && u.status == AccountStatus::Approved
                    && u.session_epoch == session.epoch =>
            {
                Some(Identity {
                    user_id: u.id,
                    username: u.username,
                    role: u.role,
                    instance_ids: u.instance_ids,
                    csrf: session.csrf,
                    session: digest.to_string(),
                })
            }
            _ => {
                self.inner.sessions.lock().await.remove(digest);
                None
            }
        }
    }

    pub async fn revoke_session(&self, digest: &str) {
        self.inner.sessions.lock().await.remove(digest);
    }

    // ---------- 登录限流 ----------

    /// 原子地预占一次登录尝试（账户 + 来源双维度）。返回 false 表示已超限。
    pub async fn reserve_login(&self, account_key: &str, source_key: &str) -> bool {
        let mut l = self.inner.limiter.lock().await;
        l.reserve_at(
            &[
                (account_key, RATE_MAX_PER_ACCOUNT),
                (source_key, RATE_MAX_PER_SOURCE),
            ],
            Instant::now(),
        )
    }

    /// 注册接口限流：调用方使用独立 `reg:` 键前缀，与登录 / 状态 / 重申共用的凭据额度隔离。
    pub async fn reserve_public(&self, account_key: &str, source_key: &str) -> bool {
        let mut l = self.inner.limiter.lock().await;
        l.reserve_at(
            &[
                (account_key, RATE_MAX_PER_ACCOUNT),
                (source_key, RATE_MAX_PER_SOURCE),
            ],
            Instant::now(),
        )
    }

    /// 登录成功：清除该账户的失败计数（来源计数保留，继续约束暴力尝试）。
    pub async fn login_succeeded(&self, account_key: &str) {
        self.inner.limiter.lock().await.clear(account_key);
    }

    // ---------- 内部 ----------

    /// 在写锁内修改账户：账户与修订号在同一事务持久化，成功后才提交到内存。
    async fn mutate<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&mut Vec<User>) -> Result<T, String>,
    {
        self.mutate_revision_checked(None, f)
            .await
            .map(|(out, _)| out)
    }

    /// 与 [`AuthStore::mutate`] 相同，但在同一写锁内可校验全局修订号，
    /// 并返回提交后的新修订号；用于权限等需要防止陈旧快照覆盖的写入。
    async fn mutate_revision_checked<F, T>(
        &self,
        expected_revision: Option<u64>,
        f: F,
    ) -> Result<(T, u64), String>
    where
        F: FnOnce(&mut Vec<User>) -> Result<T, String>,
    {
        let mut users = self.inner.users.write().await;
        if let Some(expected) = expected_revision {
            if self.revision() != expected {
                return Err("权限已被其他管理员修改，请刷新后重试".to_string());
            }
        }
        let mut next = users.clone();
        let out = f(&mut next)?;
        let path = self.inner.path.clone();
        let snapshot = next.clone();
        // 修订号与账户同事务落盘：重启后据此恢复，避免白名单同步代号回退
        let next_revision = self.inner.revision.load(Ordering::SeqCst).saturating_add(1);
        tokio::task::spawn_blocking(move || write_accounts(&path, &snapshot, next_revision))
            .await
            .map_err(|e| format!("持久化任务失败: {e}"))??;
        *users = next;
        self.inner.revision.store(next_revision, Ordering::SeqCst);
        Ok((out, next_revision))
    }

    async fn hash_password(&self, password: &str) -> Result<String, String> {
        let sem = self.inner.hashing.clone();
        let password = password.to_string();
        run_blocking_with_permit(sem, move || {
            let salt = SaltString::generate(&mut OsRng);
            Argon2::default()
                .hash_password(password.as_bytes(), &salt)
                .map(|h| h.to_string())
                .map_err(|e| format!("密码哈希失败: {e}"))
        })
        .await?
    }

    async fn verify_password(&self, hash: &str, password: &str) -> bool {
        let sem = self.inner.hashing.clone();
        let hash = hash.to_string();
        let password = password.to_string();
        run_blocking_with_permit(sem, move || {
            let Ok(parsed) = PasswordHash::new(&hash) else {
                return false;
            };
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .await
        .unwrap_or(false)
    }
}

/// 在受信号量限制的阻塞任务中执行 CPU 密集工作。
///
/// 关键点：使用 `acquire_owned` 取得自有许可并**移动进阻塞闭包**。即使调用方的 async future
/// 在等待期间被取消（如请求断开），阻塞任务仍会持有许可直到真正结束，不会出现“许可提前释放、
/// 并发超出上限”的问题。
async fn run_blocking_with_permit<T, F>(sem: Arc<Semaphore>, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = sem
        .acquire_owned()
        .await
        .map_err(|_| "密码哈希并发控制失败".to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|e| format!("密码哈希任务失败: {e}"))
}

/// 未知账户占位校验使用的固定 Argon2 哈希：首次使用时生成一次，
/// 之后每次只做一次与真实校验成本相当的 verify，避免通过响应时间枚举账户。
fn dummy_password_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(b"mcspr-timing-equalizer", &salt)
            .expect("生成占位密码哈希失败")
            .to_string()
    })
}

pub fn normalize_username(raw: &str) -> String {
    raw.trim().to_lowercase()
}

pub(crate) fn validate_username(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("用户名不能为空".to_string());
    }
    if name.chars().count() > MAX_USERNAME_LEN {
        return Err(format!("用户名最长 {MAX_USERNAME_LEN} 个字符"));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("用户名只能包含字母、数字、下划线、短横线或点".to_string());
    }
    Ok(())
}

pub(crate) fn validate_password(password: &str) -> Result<(), String> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(format!("密码至少 {MIN_PASSWORD_LEN} 位"));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(format!("密码最长 {MAX_PASSWORD_LEN} 位"));
    }
    Ok(())
}

/// 校验游戏名并返回保留原始大小写的值（仅去除首尾空白）。
pub(crate) fn normalize_minecraft_name(raw: &str) -> Result<String, String> {
    let name = raw.trim().to_string();
    let len = name.chars().count();
    if len < MIN_MC_NAME_LEN || len > MAX_MC_NAME_LEN {
        return Err(format!(
            "游戏名长度需为 {MIN_MC_NAME_LEN}~{MAX_MC_NAME_LEN} 个字符"
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("游戏名只能包含字母、数字或下划线".to_string());
    }
    Ok(name)
}

/// 游戏名唯一性：忽略大小写，涵盖已占用名、待处理的变更预留名与退休名。
fn name_taken(users: &[User], candidate: &str, exclude_user_id: Option<&str>) -> bool {
    let needle = candidate.trim().to_ascii_lowercase();
    users.iter().any(|u| {
        // 退休名对所有账户（含本人）保留：改名后不得立即复用旧名
        if u.retired_names
            .iter()
            .any(|r| r.name.trim().to_ascii_lowercase() == needle)
        {
            return true;
        }
        if exclude_user_id == Some(u.id.as_str()) {
            return false;
        }
        let current = u
            .minecraft_name
            .as_deref()
            .map(|n| n.trim().to_ascii_lowercase() == needle)
            .unwrap_or(false);
        let reserved = u
            .pending_name_change
            .as_ref()
            .map(|r| r.new_name.trim().to_ascii_lowercase() == needle)
            .unwrap_or(false);
        current || reserved
    })
}

/// 从账户列表提取已批准且启用的具名授权（纯函数，供快照复用）。
fn named_grants(users: &[User]) -> Vec<(String, String, Vec<String>, bool)> {
    users
        .iter()
        .filter(|u| u.status == AccountStatus::Approved && u.enabled)
        .filter_map(|u| {
            u.minecraft_name.clone().map(|name| {
                (
                    u.id.clone(),
                    name,
                    u.instance_ids.clone(),
                    u.role == Role::Admin,
                )
            })
        })
        .collect()
}

/// 全部实例确认移除后才释放旧游戏名。
fn retired_releasable<F>(r: &RetiredName, status_of: &F) -> bool
where
    F: Fn(&str) -> RetiredNameStatus,
{
    status_of(&r.name) == RetiredNameStatus::Removed
}

fn registration_application(u: &User) -> Application {
    Application {
        id: u.id.clone(),
        kind: "registration".to_string(),
        user_id: u.id.clone(),
        username: u.username.clone(),
        minecraft_name: u.minecraft_name.clone(),
        reason: u.application_reason.clone(),
        status: u.status,
        revision: u.application_revision,
        rejection_reason: u.rejection_reason.clone(),
        created_at: u.created_at.clone(),
    }
}

fn name_change_application(u: &User, r: &NameChangeRequest) -> Application {
    Application {
        id: r.id.clone(),
        kind: "name_change".to_string(),
        user_id: u.id.clone(),
        username: u.username.clone(),
        minecraft_name: Some(r.new_name.clone()),
        reason: r.reason.clone(),
        status: r.status,
        revision: r.revision,
        rejection_reason: r.rejection_reason.clone(),
        created_at: r.created_at.clone(),
    }
}

fn push_name_history(u: &mut User, req: NameChangeRequest) {
    u.name_change_history.push(req);
    if u.name_change_history.len() > MAX_NAME_HISTORY {
        let overflow = u.name_change_history.len() - MAX_NAME_HISTORY;
        u.name_change_history.drain(0..overflow);
    }
}

fn dedup_ids(ids: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim().to_string();
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

fn session_digest(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn write_accounts(path: &Path, users: &[User], revision: u64) -> Result<(), String> {
    let file = AccountFile {
        schema_version: SCHEMA_VERSION,
        revision,
        users: users.to_vec(),
    };
    let data = serde_json::to_string_pretty(&file).map_err(|e| format!("序列化账户失败: {e}"))?;
    let parent = path.parent().ok_or("账户路径无效")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建账户目录失败: {e}"))?;
    #[cfg(unix)]
    {
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts
            .open(&tmp)
            .map_err(|e| format!("写入账户临时文件失败: {e}"))?;
        use std::io::Write;
        f.write_all(data.as_bytes())
            .map_err(|e| format!("写入账户临时文件失败: {e}"))?;
        f.sync_all().map_err(|e| format!("同步账户文件失败: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("替换账户文件失败: {e}"))?;
    Ok(())
}

// ---------- 本地交互式 CLI ----------

use std::io::{BufRead, IsTerminal, Write};

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

/// TTY 下隐藏输入；从管道读取（自动化测试）时按普通行读取。
fn prompt_password(prompt: &str) -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        Ok(rpassword::prompt_password(prompt)?)
    } else {
        prompt_line(prompt)
    }
}

fn prompt_password_twice(prompt: &str) -> anyhow::Result<String> {
    let first = prompt_password(prompt)?;
    if first.is_empty() {
        anyhow::bail!("密码不能为空");
    }
    let second = prompt_password("请再次输入密码: ")?;
    if first != second {
        anyhow::bail!("两次输入的密码不一致");
    }
    Ok(first)
}

/// 创建首个管理员；账户存储已存在（含损坏 / 不受支持文件）时拒绝执行，避免覆盖现有账户。
///
/// 会尝试获取账户存储锁：若面板服务正在运行则拒绝。
pub async fn cli_init_admin(data_dir: &str) -> anyhow::Result<()> {
    let store = AuthStore::load(data_dir)?;
    if store.has_users().await {
        anyhow::bail!(
            "账户存储已存在且非空（{}），为避免覆盖现有账户已拒绝初始化；如需重置管理员密码请使用 --reset-admin-password",
            store.path().display()
        );
    }
    let username = prompt_line("管理员用户名: ")?;
    let password = prompt_password_twice("管理员密码: ")?;
    let user = store
        .create_user(&username, &password, Role::Admin, Vec::new())
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "已创建管理员账户「{}」。账户文件：{}",
        user.username,
        store.path().display()
    );
    Ok(())
}

/// 重置管理员密码。
///
/// 需要面板服务已停止：账户存储锁会拒绝与服务并发运行。账户存储缺失 / 为空时拒绝执行。
pub async fn cli_reset_admin_password(data_dir: &str) -> anyhow::Result<()> {
    let store = AuthStore::load(data_dir)?;
    if !store.has_users().await {
        anyhow::bail!(
            "账户存储不存在或为空（{}），请先使用 --init-admin 创建管理员",
            store.path().display()
        );
    }
    let username = prompt_line("管理员用户名: ")?;
    let user = store
        .find_by_username(&username)
        .await
        .ok_or_else(|| anyhow::anyhow!("未找到账户「{}」", normalize_username(&username)))?;
    if user.role != Role::Admin {
        anyhow::bail!("账户「{}」不是管理员，拒绝重置", user.username);
    }
    let password = prompt_password_twice("新密码: ")?;
    store
        .reset_password(&user.id, &password)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "已重置管理员「{}」的密码。请重启面板服务后使用新密码登录（本地重置要求服务已停止）。",
        user.username
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TempDir {
        let dir = std::env::temp_dir().join(format!("mcspr_auth_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    async fn store() -> (AuthStore, TempDir) {
        let dir = temp_dir();
        let store = AuthStore::load(&dir.0).unwrap();
        (store, dir)
    }

    #[test]
    fn username_normalization_and_validation() {
        assert_eq!(normalize_username("  Alice  "), "alice");
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("").is_err());
        assert!(validate_username("a b").is_err());
        assert!(validate_username(&"x".repeat(40)).is_err());
        assert!(validate_password("short").is_err());
        assert!(validate_password("longenough").is_ok());
    }

    #[tokio::test]
    async fn create_duplicate_and_authenticate() {
        let (s, _dir) = store().await;
        s.create_user("Alice", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        assert!(s
            .create_user("alice", "password123", Role::User, vec![])
            .await
            .is_err());
        assert!(s.verify_credentials("alice", "password123").await.is_some());
        assert!(s.verify_credentials("alice", "wrong").await.is_none());
        assert!(s
            .verify_credentials("nobody", "password123")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn last_admin_is_protected() {
        let (s, _dir) = store().await;
        let admin = s
            .create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        assert!(s.set_enabled(&admin.id, false).await.is_err());
        assert!(s.set_role(&admin.id, Role::User).await.is_err());
        assert!(s.delete(&admin.id).await.is_err());
        let second = s
            .create_user("root2", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        // 存在第二个启用管理员时，可禁用第一个
        assert!(s.set_enabled(&admin.id, false).await.is_ok());
        // 此时 second 成为唯一启用管理员，删除被拒绝
        assert!(s.delete(&second.id).await.is_err());
        // 重新启用 admin 后即可删除 second
        assert!(s.set_enabled(&admin.id, true).await.is_ok());
        assert!(s.delete(&second.id).await.is_ok());
    }

    #[tokio::test]
    async fn update_user_is_atomic_for_role_and_enabled() {
        let (s, _dir) = store().await;
        let admin = s
            .create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        // 单独降级 + 禁用最后管理员（一次原子写入）必须整体被拒
        assert!(s
            .update_user(&admin.id, Some(false), Some(Role::User))
            .await
            .is_err());
        // 账户未被部分修改
        let after = s.get(&admin.id).await.unwrap();
        assert_eq!(after.role, Role::Admin);
        assert!(after.enabled);
        // 同时设置角色与启用状态可成功（存在第二管理员时）
        let second = s
            .create_user("root2", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        let updated = s
            .update_user(&admin.id, Some(false), Some(Role::User))
            .await
            .unwrap();
        assert_eq!(updated.role, Role::User);
        assert!(!updated.enabled);
        let _ = second;
    }

    #[tokio::test]
    async fn session_revoked_by_password_change_and_role_change() {
        let (s, _dir) = store().await;
        let user = s
            .create_user("bob", "password123", Role::User, vec![])
            .await
            .unwrap();
        let (token, _csrf) = s.start_session(&user.id, 0).await.unwrap();
        assert!(s.session_identity(&token).await.is_some());
        s.change_password(&user.id, "password123", "newpassword1")
            .await
            .unwrap();
        assert!(s.session_identity(&token).await.is_none());
        let epoch = s.verify_credentials("bob", "newpassword1").await.unwrap().1;
        let (token2, _) = s.start_session(&user.id, epoch).await.unwrap();
        s.set_role(&user.id, Role::Admin).await.unwrap();
        assert!(s.session_identity(&token2).await.is_none());
    }

    #[tokio::test]
    async fn session_start_rejects_epoch_changed_by_reset() {
        let (s, _dir) = store().await;
        let user = s
            .create_user("bob", "password123", Role::User, vec![])
            .await
            .unwrap();
        // 校验得到的凭据纪元
        let (_, epoch) = s.verify_credentials("bob", "password123").await.unwrap();
        // 期间发生重置（提升纪元）
        s.reset_password(&user.id, "anotherpass1").await.unwrap();
        // 使用旧纪元创建会话必须失败
        assert!(s.start_session(&user.id, epoch).await.is_none());
        let (_, new_epoch) = s.verify_credentials("bob", "anotherpass1").await.unwrap();
        assert!(new_epoch != epoch);
        assert!(s.start_session(&user.id, new_epoch).await.is_some());
    }

    #[tokio::test]
    async fn password_change_recheck_prevents_overwriting_reset() {
        let (s, _dir) = store().await;
        let user = s
            .create_user("bob", "password123", Role::User, vec![])
            .await
            .unwrap();
        let (hash, epoch) = {
            // 快照当前哈希与纪元，模拟“已通过旧密码校验”
            let users = s.inner.users.read().await;
            let u = users.iter().find(|u| u.id == user.id).unwrap();
            (u.password_hash.clone(), u.session_epoch)
        };
        // 期间发生重置
        s.reset_password(&user.id, "anotherpass1").await.unwrap();
        // 用过期快照写入必须被拒
        let err = s
            .apply_password_change(&user.id, hash, epoch, "newhash".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("已变更"));
        // 重置后的密码仍然有效，未被覆盖
        assert!(s.verify_credentials("bob", "anotherpass1").await.is_some());
    }

    #[test]
    fn rate_limiter_expires_and_recovers() {
        let mut l = RateLimiter::default();
        let now = Instant::now();
        for _ in 0..RATE_MAX_PER_ACCOUNT {
            assert!(l.reserve_at(&[("acct", RATE_MAX_PER_ACCOUNT)], now));
        }
        assert!(!l.reserve_at(&[("acct", RATE_MAX_PER_ACCOUNT)], now));
        // 窗口过期后恢复
        let later = now + RATE_WINDOW + Duration::from_secs(1);
        assert!(l.reserve_at(&[("acct", RATE_MAX_PER_ACCOUNT)], later));
        assert_eq!(l.hits.get("acct").map(|q| q.len()), Some(1));
    }

    #[test]
    fn rate_limiter_bounded_pressure_preserves_active_limits() {
        let mut l = RateLimiter::default();
        let now = Instant::now();
        // 先登记一个受限键并打满
        for _ in 0..RATE_MAX_PER_ACCOUNT {
            assert!(l.reserve_at(&[("victim", RATE_MAX_PER_ACCOUNT)], now));
        }
        assert!(!l.reserve_at(&[("victim", RATE_MAX_PER_ACCOUNT)], now));
        // 用大量伪造键填满表
        for i in 0..RATE_MAX_KEYS {
            l.reserve_at(&[(format!("k{i}").as_str(), RATE_MAX_PER_ACCOUNT)], now);
        }
        // 达到上限后新键不登记，但既有活动限制保留
        let cap = l.hits.len();
        assert!(cap <= RATE_MAX_KEYS);
        assert!(!l.reserve_at(&[("victim", RATE_MAX_PER_ACCOUNT)], now));
        assert_eq!(l.hits.len(), cap);
    }

    #[tokio::test]
    async fn concurrent_reserve_is_atomic() {
        let (s, _dir) = store().await;
        let account = "u:target";
        let source = "s:peer";
        let ok = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..50 {
            let s = s.clone();
            let ok = ok.clone();
            tasks.push(tokio::spawn(async move {
                if s.reserve_login(account, source).await {
                    ok.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            ok.load(std::sync::atomic::Ordering::SeqCst),
            RATE_MAX_PER_ACCOUNT,
            "并发尝试必须精确受账户额度限制"
        );
    }

    #[tokio::test]
    async fn cancelled_future_keeps_hash_permit_until_blocking_finishes() {
        let sem = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let sem_task = sem.clone();
        let handle = tokio::spawn(async move {
            run_blocking_with_permit(sem_task, move || {
                let _ = started_tx.send(());
                std::thread::sleep(Duration::from_millis(300));
                let _ = done_tx.send(());
            })
            .await
        });
        started_rx.await.unwrap();
        // 取消等待中的 async future（阻塞任务仍在运行并持有许可）
        handle.abort();
        let _ = handle.await;
        assert_eq!(sem.available_permits(), 0, "取消后阻塞任务必须继续持有许可");
        done_rx.await.unwrap();
        // 阻塞任务结束后许可才归还
        let mut released = false;
        for _ in 0..100 {
            if sem.available_permits() == 1 {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(released, "阻塞任务结束后许可应归还");
    }

    #[tokio::test]
    async fn corrupt_store_is_rejected() {
        let dir = temp_dir();
        let auth = dir.0.join("auth");
        std::fs::create_dir_all(&auth).unwrap();
        std::fs::write(auth.join("users.json"), b"{ not json").unwrap();
        assert!(AuthStore::load(&dir.0).is_err());
    }

    #[tokio::test]
    async fn unsupported_schema_is_rejected() {
        for version in [0u32, 3, 999] {
            let dir = temp_dir();
            let auth = dir.0.join("auth");
            std::fs::create_dir_all(&auth).unwrap();
            let body = format!("{{\"schema_version\":{version},\"users\":[]}}");
            std::fs::write(auth.join("users.json"), body).unwrap();
            assert!(
                AuthStore::load(&dir.0).is_err(),
                "schema_version={version} 应被拒绝"
            );
        }
        // 受支持版本（含迁移来源 v1）可加载
        for version in [1u32, 2] {
            let dir = temp_dir();
            let auth = dir.0.join("auth");
            std::fs::create_dir_all(&auth).unwrap();
            let body = format!("{{\"schema_version\":{version},\"users\":[]}}");
            std::fs::write(auth.join("users.json"), body).unwrap();
            assert!(AuthStore::load(&dir.0).is_ok());
        }
    }

    #[tokio::test]
    async fn store_lock_rejects_second_active_store() {
        let dir = temp_dir();
        let first = AuthStore::load(&dir.0).unwrap();
        assert!(
            AuthStore::load(&dir.0).is_err(),
            "同一数据目录不允许第二个活动存储"
        );
        drop(first);
        // 释放后可以再次获取
        assert!(AuthStore::load(&dir.0).is_ok());
    }

    #[tokio::test]
    async fn legacy_v1_account_migrates_to_approved() {
        let dir = temp_dir();
        let auth = dir.0.join("auth");
        std::fs::create_dir_all(&auth).unwrap();
        let body = r#"{"schema_version":1,"users":[{"id":"u1","username":"old","password_hash":"x","role":"user","enabled":true,"instance_ids":[],"session_epoch":0,"created_at":"","updated_at":""}]}"#;
        std::fs::write(auth.join("users.json"), body).unwrap();
        let store = AuthStore::load(&dir.0).unwrap();
        // 旧 schema 无 revision 字段：默认为 0，仍可加载
        assert_eq!(store.revision(), 0);
        let users = store.list().await;
        let old = users.iter().find(|u| u.username == "old").unwrap();
        assert_eq!(old.status, AccountStatus::Approved);
        assert!(old.minecraft_name.is_none());
    }

    #[tokio::test]
    async fn registration_creates_pending_unique_user() {
        let (s, _dir) = store().await;
        s.create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        let pending = s
            .register_pending_user("Alice", "password123", "Steve", "请批准")
            .await
            .unwrap();
        assert_eq!(pending.status, AccountStatus::Pending);
        assert_eq!(pending.role, Role::User);
        assert_eq!(pending.minecraft_name.as_deref(), Some("Steve"));
        // 大小写不敏感的游戏名占用
        assert!(s
            .register_pending_user("bob", "password123", "steve", "x")
            .await
            .is_err());
        // 用户名重复
        assert!(s
            .register_pending_user("alice", "password123", "Alex", "x")
            .await
            .is_err());
        // 待审批账户仍可用凭据查询（登录接口再判定状态）
        let (user, _epoch) = s.verify_credentials("alice", "password123").await.unwrap();
        assert_eq!(user.status, AccountStatus::Pending);
    }

    #[tokio::test]
    async fn name_change_reserves_name_and_revision_guards_approval() {
        let (s, _dir) = store().await;
        s.create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        let u = s
            .register_pending_user("alice", "password123", "Alice", "reason")
            .await
            .unwrap();
        s.approve_application(&u.id, 1, vec![], "root")
            .await
            .unwrap();
        let req = s.request_name_change(&u.id, "Neo", "改名").await.unwrap();
        // 新名被预留（大小写不敏感）
        assert!(s
            .register_pending_user("bob", "password123", "neo", "x")
            .await
            .is_err());
        // 错误修订号被拒
        assert!(s
            .approve_application(&req.id, 99, vec![], "root")
            .await
            .is_err());
        // 正确修订号批准：用户名与登录不变，游戏名更新
        s.approve_application(&req.id, req.revision, vec![], "root")
            .await
            .unwrap();
        let after = s.get(&u.id).await.unwrap();
        assert_eq!(after.username, "alice");
        assert_eq!(after.minecraft_name.as_deref(), Some("Neo"));
        // 旧名退休保留：改名后不能立即复用
        assert!(s
            .register_pending_user("carol", "password123", "Alice", "x")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reject_and_resubmit_bumps_revision() {
        let (s, _dir) = store().await;
        s.create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        let u = s
            .register_pending_user("dave", "password123", "Dave", "reason")
            .await
            .unwrap();
        s.reject_application(&u.id, 1, "资料不全", "root")
            .await
            .unwrap();
        // 被拒后仍可用旧密码查询状态
        assert!(s.verify_credentials("dave", "password123").await.is_some());
        // 重申回到待审批，修订号自增
        s.resubmit_application(&u.id, "Dave2", "补充资料")
            .await
            .unwrap();
        let after = s.get(&u.id).await.unwrap();
        assert_eq!(after.status, AccountStatus::Pending);
        assert_eq!(after.minecraft_name.as_deref(), Some("Dave2"));
        // 旧修订号批准被拒
        assert!(s
            .approve_application(&u.id, 1, vec![], "root")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn instance_grants_are_scoped_and_revision_checked() {
        let (s, _dir) = store().await;
        let a = s
            .create_user("a", "password123", Role::User, vec!["inst-x".to_string()])
            .await
            .unwrap();
        let b = s
            .create_user("b", "password123", Role::User, vec![])
            .await
            .unwrap();
        let rev = s.instance_permission_revision("inst-y").await;
        let next = s
            .set_instance_grants_checked("inst-y", rev, vec![a.id.clone(), b.id.clone()])
            .await
            .unwrap();
        assert_eq!(next, rev + 1);
        let a_after = s.get(&a.id).await.unwrap();
        assert!(a_after.instance_ids.iter().any(|i| i == "inst-y"));
        assert!(a_after.instance_ids.iter().any(|i| i == "inst-x"));
        // 陈旧修订号冲突
        assert!(s
            .set_instance_grants_checked("inst-y", rev, vec![a.id.clone()])
            .await
            .is_err());
        // 仅改变 inst-y：a 失去 inst-y，b 获得，inst-x 保留
        s.set_instance_grants_checked("inst-y", next, vec![b.id.clone()])
            .await
            .unwrap();
        let a_final = s.get(&a.id).await.unwrap();
        assert!(!a_final.instance_ids.iter().any(|i| i == "inst-y"));
        assert!(a_final.instance_ids.iter().any(|i| i == "inst-x"));
    }

    #[tokio::test]
    async fn approved_named_grants_and_revision_track_changes() {
        let (s, _dir) = store().await;
        s.create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        let u = s
            .register_pending_user("alice", "password123", "Alice", "r")
            .await
            .unwrap();
        let before = s.revision();
        s.approve_application(&u.id, 1, vec!["inst-a".to_string()], "root")
            .await
            .unwrap();
        let grants = s.approved_named_grants().await;
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].1, "Alice");
        assert_eq!(grants[0].2, vec!["inst-a".to_string()]);
        assert!(!grants[0].3);
        assert!(s.revision() > before);
    }

    #[tokio::test]
    async fn revision_persists_across_restart_including_instance_grants() {
        let dir = temp_dir();
        let persisted = {
            let store = AuthStore::load(&dir.0).unwrap();
            let root = store
                .create_user("root", "password123", Role::Admin, vec![])
                .await
                .unwrap();
            let rev = store.instance_permission_revision("inst-a").await;
            store
                .set_instance_grants_checked("inst-a", rev, vec![root.id.clone()])
                .await
                .unwrap();
            assert!(store.revision() >= 2);
            store.revision()
        };
        // 重载后修订号不得回退为 0，否则白名单同步会因代号陈旧被跳过
        let reloaded = AuthStore::load(&dir.0).unwrap();
        assert_eq!(reloaded.revision(), persisted);
        assert_eq!(
            reloaded
                .approved_named_grants_with_revision()
                .await
                .revision,
            persisted
        );
    }

    #[tokio::test]
    async fn retired_old_name_blocks_register_and_rebind_until_released() {
        let (s, _dir) = store().await;
        s.create_user("root", "password123", Role::Admin, vec![]).await.unwrap();
        let u = s
            .register_pending_user("alice", "password123", "Alice", "r")
            .await
            .unwrap();
        s.approve_application(&u.id, 1, vec!["inst-a".to_string()], "root")
            .await
            .unwrap();
        let (revision, grants) = s.approved_grants_snapshot().await;
        assert_eq!(revision, s.revision(), "快照修订号应与当前修订号一致");
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].1, "Alice");
        assert_eq!(grants[0].2, vec!["inst-a".to_string()]);
        assert!(!grants[0].3);
        let req = s.request_name_change(&u.id, "Neo", "改名").await.unwrap();
        s.approve_application(&req.id, req.revision, vec![], "root")
            .await
            .unwrap();
        let after = s.get(&u.id).await.unwrap();
        assert_eq!(after.minecraft_name.as_deref(), Some("Neo"));

        // 旧名退休：他人注册被拒
        assert!(s
            .register_pending_user("carol", "password123", "Alice", "x")
            .await
            .is_err());
        // 本人改回旧名同样被拒
        assert!(s.request_name_change(&u.id, "Alice", "回退").await.is_err());

        // 相关实例未确认移除：保守保留
        let n = s
            .release_retired_names_if_removed(|_| RetiredNameStatus::Unknown)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(s
            .register_pending_user("carol", "password123", "Alice", "x")
            .await
            .is_err());

        // 实例仍人工保留旧名：同样保持预留
        let n = s
            .release_retired_names_if_removed(|_| RetiredNameStatus::Retained)
            .await
            .unwrap();
        assert_eq!(n, 0);

        // 相关实例确认移除后释放
        let n = s
            .release_retired_names_if_removed(|_| RetiredNameStatus::Removed)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // 本人可改回旧名
        let back = s.request_name_change(&u.id, "Alice", "回退").await.unwrap();
        s.withdraw_name_change(&u.id, &back.id).await.unwrap();
        // 释放后他人可注册旧名
        assert!(s
            .register_pending_user("carol", "password123", "Alice", "x")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn registration_requires_approved_admin() {
        let (s, _dir) = store().await;
        // 尚无已批准管理员：注册被拒，避免 pending 占存储锁死首次管理员 CLI
        assert!(s
            .register_pending_user("alice", "password123", "Alice", "r")
            .await
            .is_err());
        assert!(!s.has_users().await, "被拒的注册不得占用存储");
        s.create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        assert!(s
            .register_pending_user("alice", "password123", "Alice", "r")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn start_session_and_identity_require_approval() {
        let (s, _dir) = store().await;
        s.create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        let pending = s
            .register_pending_user("alice", "password123", "Alice", "r")
            .await
            .unwrap();
        assert_eq!(pending.status, AccountStatus::Pending);
        // 待审批账户不能创建会话
        assert!(s.start_session(&pending.id, 0).await.is_none());
        s.approve_application(&pending.id, 1, vec![], "root")
            .await
            .unwrap();
        let (token, _) = s.start_session(&pending.id, 0).await.unwrap();
        assert!(s.session_identity(&token).await.is_some());
        // 会话建立后再回到非批准状态：身份解析必须拒绝并撤销会话
        {
            let mut users = s.inner.users.write().await;
            let u = users.iter_mut().find(|u| u.id == pending.id).unwrap();
            u.status = AccountStatus::Pending;
        }
        assert!(s.session_identity(&token).await.is_none());
    }

    #[tokio::test]
    async fn instance_permission_revision_invalidated_by_other_grants() {
        let (s, _dir) = store().await;
        let bob = s
            .create_user("bob", "password123", Role::User, vec![])
            .await
            .unwrap();
        let stale = s.instance_permission_revision("inst-a").await;
        // 通过其他接口（直接授权）修改同一实例的权限
        s.set_instances(&bob.id, vec!["inst-a".to_string()])
            .await
            .unwrap();
        // 旧权限快照不再有效，避免无声回滚
        let err = s
            .set_instance_grants_checked("inst-a", stale, vec![])
            .await
            .unwrap_err();
        assert!(err.contains("已被其他管理员修改"));
        let after = s.get(&bob.id).await.unwrap();
        assert!(after.instance_ids.iter().any(|i| i == "inst-a"));
    }

    #[tokio::test]
    async fn unknown_user_verification_uses_valid_dummy_hash() {
        let (s, _dir) = store().await;
        assert!(s.verify_credentials("ghost", "password123").await.is_none());
        assert!(PasswordHash::new(dummy_password_hash()).is_ok());
    }

    #[test]
    fn rate_limiter_fails_closed_and_recovers() {
        let mut l = RateLimiter::default();
        let now = Instant::now();
        for i in 0..RATE_MAX_KEYS {
            assert!(l.reserve_at(&[(format!("k{i}").as_str(), RATE_MAX_PER_ACCOUNT)], now));
        }
        assert_eq!(l.hits.len(), RATE_MAX_KEYS);
        // 达到上限后新键失败关闭（不再放行）
        assert!(!l.reserve_at(&[("fresh", RATE_MAX_PER_ACCOUNT)], now));
        assert!(!l.hits.contains_key("fresh"));
        // 窗口过期后全局清理，新键可重新登记
        let later = now + RATE_WINDOW + Duration::from_secs(1);
        assert!(l.reserve_at(&[("fresh", RATE_MAX_PER_ACCOUNT)], later));
        assert_eq!(l.hits.len(), 1);
    }
}
