pub mod backup;
pub mod game_backup;
pub mod files;
pub mod javainstall;
pub mod loaders;
pub mod moddb;
pub mod modpack;
pub mod mods;
pub mod process;
pub mod properties;
pub mod tasks;
pub mod users;
pub mod vanilla;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub java_path: Option<String>,
    #[serde(default = "default_min_ram")]
    pub min_ram_mb: u32,
    #[serde(default = "default_max_ram")]
    pub max_ram_mb: u32,
    #[serde(default)]
    pub jar: Option<String>,
    #[serde(default)]
    pub jvm_args: String,
    #[serde(default)]
    pub auto_restart: bool,
    /// 创建时选择的 Minecraft 版本（用于模组下载的默认过滤）
    #[serde(default)]
    pub mc_version: Option<String>,
    /// 创建时选择的模组加载器（fabric/quilt/forge/neoforge）
    #[serde(default)]
    pub mod_loader: Option<String>,
    /// 面板启动时自动拉起此实例
    #[serde(default)]
    pub auto_start_on_boot: bool,
}

fn default_min_ram() -> u32 {
    1024
}
fn default_max_ram() -> u32 {
    4096
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Stopped => "stopped",
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Stopping => "stopping",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub ts: String,
    pub line: String,
}

pub struct InstanceRuntime {
    pub meta: RwLock<InstanceMeta>,
    pub dir: PathBuf,
    pub status: Mutex<Status>,
    pub pid: AtomicU32,
    pub started_at: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    pub stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    pub stopping: Arc<AtomicBool>,
    pub ready: AtomicBool,
    pub players: Mutex<Vec<String>>,
    /// 崩溃时间戳（用于重启风暴熔断）
    pub crash_times: Mutex<VecDeque<std::time::Instant>>,
    /// 触发重启风暴后置位（不再自动重启，需手动启动）
    pub restart_storm: AtomicBool,
    /// 玩家在线时长（秒, 会话数）
    pub playtime: Mutex<std::collections::BTreeMap<String, (u64, u32)>>,
    /// 进行中的玩家会话（join 时间）
    pub open_sessions: Mutex<std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>>,
    /// CPU/内存历史采样 (时间戳, cpu%, mem_mb)
    pub metrics: Mutex<VecDeque<(i64, f32, f64)>>,
    /// TPS/MSPT 采样结果
    pub tps: Mutex<Option<Value>>,
    /// 复用的 RCON 连接（TPS 采样与远程命令）；服务端会为每次新建连接打一行日志
    pub rcon: std::sync::Mutex<Option<crate::rcon::Session>>,
    pub log_buf: Mutex<VecDeque<LogLine>>,
    pub log_tx: broadcast::Sender<LogLine>,
    pub next_seq: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub status: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub players: usize,
    pub player_names: Vec<String>,
    pub jar: Option<String>,
    pub java_path: Option<String>,
    pub min_ram_mb: u32,
    pub max_ram_mb: u32,
    pub jvm_args: String,
    pub auto_restart: bool,
    pub auto_start_on_boot: bool,
    pub restart_storm: bool,
    pub tps: Option<serde_json::Value>,
    pub mc_version: Option<String>,
    pub mod_loader: Option<String>,
    pub eula_accepted: bool,
}

impl InstanceRuntime {
    pub fn new(meta: InstanceMeta, dir: PathBuf) -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        // 预填 latest.log 末尾，面板重启后控制台仍有上下文
        let mut buf: VecDeque<LogLine> = VecDeque::new();
        for line in files::tail_lines(&dir.join("logs").join("latest.log"), 200) {
            let seq = buf.back().map(|l| l.seq + 1).unwrap_or(1);
            buf.push_back(LogLine {
                seq,
                ts: String::new(),
                line,
            });
        }
        let next_seq = buf.back().map(|l| l.seq).unwrap_or(0);
        // 恢复玩家在线时长统计
        let playtime: std::collections::BTreeMap<String, (u64, u32)> =
            std::fs::read_to_string(dir.join("player-stats.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
        Arc::new(Self {
            meta: RwLock::new(meta),
            dir,
            status: Mutex::new(Status::Stopped),
            pid: AtomicU32::new(0),
            started_at: Mutex::new(None),
            stdin: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
            ready: AtomicBool::new(false),
            players: Mutex::new(Vec::new()),
            crash_times: Mutex::new(VecDeque::new()),
            restart_storm: AtomicBool::new(false),
            playtime: Mutex::new(playtime),
            open_sessions: Mutex::new(std::collections::HashMap::new()),
            metrics: Mutex::new(VecDeque::new()),
            tps: Mutex::new(None),
            rcon: std::sync::Mutex::new(None),
            log_buf: Mutex::new(buf),
            log_tx: tx,
            next_seq: AtomicU64::new(next_seq),
        })
    }

    pub async fn persist(&self) -> ApiResult<()> {
        let meta = self.meta.read().await.clone();
        tokio::fs::write(self.dir.join("instance.json"), serde_json::to_string_pretty(&meta)?).await?;
        Ok(())
    }

    pub async fn display_status(&self) -> &'static str {
        let st = *self.status.lock().await;
        if st == Status::Starting && self.ready.load(Ordering::SeqCst) {
            Status::Running.as_str()
        } else {
            st.as_str()
        }
    }

    pub async fn summary(&self) -> InstanceSummary {
        let meta = self.meta.read().await.clone();
        let players = self.players.lock().await;
        InstanceSummary {
            id: meta.id,
            name: meta.name,
            created_at: meta.created_at,
            status: self.display_status().await.to_string(),
            pid: self.pid.load(Ordering::SeqCst),
            uptime_secs: uptime(self).await,
            players: players.len(),
            player_names: players.clone(),
            jar: meta.jar,
            java_path: meta.java_path,
            min_ram_mb: meta.min_ram_mb,
            max_ram_mb: meta.max_ram_mb,
            jvm_args: meta.jvm_args,
            auto_restart: meta.auto_restart,
            auto_start_on_boot: meta.auto_start_on_boot,
            restart_storm: self.restart_storm.load(Ordering::SeqCst),
            tps: self.tps.lock().await.clone(),
            mc_version: meta.mc_version,
            mod_loader: meta.mod_loader,
            eula_accepted: eula_accepted(&self.dir),
        }
    }
}

pub async fn uptime(rt: &InstanceRuntime) -> u64 {
    match *rt.started_at.lock().await {
        Some(t) => (chrono::Utc::now() - t).num_seconds().max(0) as u64,
        None => 0,
    }
}

pub fn eula_accepted(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("eula.txt")) {
        Ok(s) => s.lines().any(|l| {
            let l = l.trim();
            (l.starts_with("eula=") || l.starts_with("eula ="))
                && l.split('=')
                    .nth(1)
                    .map(|v| v.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
        }),
        Err(_) => false,
    }
}

pub fn accept_eula_sync(dir: &Path) -> std::io::Result<()> {
    let path = dir.join("eula.txt");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|s| s.to_string()).collect();
    let mut replaced = false;
    for l in lines.iter_mut() {
        let t = l.trim_start();
        if (t.starts_with("eula=") || t.starts_with("eula =")) && l.contains('=') {
            *l = "eula=true".into();
            replaced = true;
        }
    }
    if !replaced {
        lines.push("# 已由 MCS Panel 同意".into());
        lines.push("eula=true".into());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(&path, out)
}

pub async fn get_instance(state: &AppState, id: &str) -> ApiResult<Arc<InstanceRuntime>> {
    state
        .instances
        .read()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("实例不存在"))
}

pub fn scan_instances(dir: &Path) -> HashMap<String, Arc<InstanceRuntime>> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let meta_path = p.join("instance.json");
        if !meta_path.exists() {
            tracing::warn!("跳过缺少 instance.json 的目录: {}", p.display());
            continue;
        }
        match std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<InstanceMeta>(&s).ok())
        {
            Some(meta) => {
                let id = meta.id.clone();
                map.insert(id, InstanceRuntime::new(meta, p));
            }
            None => tracing::warn!("instance.json 解析失败: {}", meta_path.display()),
        }
    }
    map
}
