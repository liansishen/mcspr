use crate::audit::AuditEntry;
use crate::config::PanelConfig;
use crate::instance::InstanceRuntime;
use crate::jobs::Job;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct AppStateInner {
    pub config: RwLock<PanelConfig>,
    /// 账户 / 会话存储：路径在启动时由数据目录确定，不随运行时设置变化
    pub auth: crate::auth::AuthStore,
    pub instances: RwLock<HashMap<String, Arc<InstanceRuntime>>>,
    pub jobs: std::sync::Mutex<HashMap<String, Job>>,
    /// 任务 / 操作持久化目录（启动时确定，不随运行时设置变化）
    pub tasks_dir: PathBuf,
    /// 运行中任务日志落盘节流时间戳（毫秒）
    pub job_persist_at: std::sync::atomic::AtomicU64,
    /// 写操作幂等登记（X-Operation-ID）
    pub operations: crate::operations::OperationStore,
    pub sys: std::sync::Mutex<sysinfo::System>,
    pub http: reqwest::Client,
    pub mc_versions: std::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>>,
    pub forge_builds: std::sync::Mutex<Option<(std::time::Instant, Vec<String>)>>,
    pub audit: Mutex<VecDeque<AuditEntry>>,
    /// 实例目录大小缓存 (计算时间, 总字节)
    pub size_cache: std::sync::Mutex<HashMap<String, (std::time::Instant, u64)>>,
    /// 告警去重 (key, 上次发送时间)
    pub alert_dedup: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// 实例级任务互斥：正在执行克隆/重装/更新/备份等整体操作的实例 ID
    pub busy: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl AppStateInner {
    /// 尝试占用实例（防止克隆/重装/更新等整体操作并发执行）
    pub fn acquire_busy(&self, id: &str) -> bool {
        self.busy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string())
    }

    pub fn release_busy(&self, id: &str) {
        self.busy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
}

#[derive(Clone)]
pub struct AppState(pub Arc<AppStateInner>);

pub struct BusyGuard {
    state: AppState,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.state.release_busy(&self.id);
    }
}

impl std::ops::Deref for AppState {
    type Target = AppStateInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AppState {
    pub fn busy_guard(&self, id: &str) -> Option<BusyGuard> {
        self.acquire_busy(id).then(|| BusyGuard {
            state: self.clone(),
            id: id.to_string(),
        })
    }
    pub async fn new(cfg: PanelConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cfg.instances_dir())?;
        let data_dir = cfg.data_dir.clone();
        let tasks_dir = std::path::Path::new(&data_dir).join("tasks");
        std::fs::create_dir_all(&tasks_dir)?;
        let auth = crate::auth::AuthStore::load(&data_dir)?;
        let instances = crate::instance::scan_instances(&cfg.instances_dir(), cfg.console_buffer_lines);
        tracing::info!("已加载 {} 个实例", instances.len());
        let http = reqwest::Client::builder()
            .user_agent("MCS-Panel/0.1")
            .build()?;
        let state = Self(Arc::new(AppStateInner {
            config: RwLock::new(cfg),
            auth,
            instances: RwLock::new(instances),
            jobs: std::sync::Mutex::new(HashMap::new()),
            tasks_dir,
            job_persist_at: std::sync::atomic::AtomicU64::new(0),
            operations: crate::operations::OperationStore::load(&data_dir),
            sys: std::sync::Mutex::new(sysinfo::System::new()),
            http,
            mc_versions: std::sync::Mutex::new(None),
            forge_builds: std::sync::Mutex::new(None),
            audit: Mutex::new(VecDeque::new()),
            size_cache: std::sync::Mutex::new(HashMap::new()),
            alert_dedup: std::sync::Mutex::new(HashMap::new()),
            busy: std::sync::Mutex::new(std::collections::HashSet::new()),
        }));
        // 崩溃恢复：运行中任务/操作标记为中断，不自动重放
        crate::jobs::restore(&state);
        crate::operations::restore(&state);
        // 清理上次运行遗留的上传缓存
        crate::jobs::cleanup_stale_uploads(&state);
        Ok(state)
    }
}
