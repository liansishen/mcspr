use crate::audit::AuditEntry;
use crate::config::PanelConfig;
use crate::instance::InstanceRuntime;
use crate::jobs::Job;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct AppStateInner {
    pub config: RwLock<PanelConfig>,
    pub instances: RwLock<HashMap<String, Arc<InstanceRuntime>>>,
    pub jobs: std::sync::Mutex<HashMap<String, Job>>,
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

impl std::ops::Deref for AppState {
    type Target = AppStateInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AppState {
    pub async fn new(cfg: PanelConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cfg.instances_dir())?;
        let instances = crate::instance::scan_instances(&cfg.instances_dir());
        tracing::info!("已加载 {} 个实例", instances.len());
        let http = reqwest::Client::builder()
            .user_agent("MCS-Panel/0.1")
            .build()?;
        Ok(Self(Arc::new(AppStateInner {
            config: RwLock::new(cfg),
            instances: RwLock::new(instances),
            jobs: std::sync::Mutex::new(HashMap::new()),
            sys: std::sync::Mutex::new(sysinfo::System::new()),
            http,
            mc_versions: std::sync::Mutex::new(None),
            forge_builds: std::sync::Mutex::new(None),
            audit: Mutex::new(VecDeque::new()),
            size_cache: std::sync::Mutex::new(HashMap::new()),
            alert_dedup: std::sync::Mutex::new(HashMap::new()),
            busy: std::sync::Mutex::new(std::collections::HashSet::new()),
        })))
    }
}
