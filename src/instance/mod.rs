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
use serde_json::{json, Value};
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
    /// 公告 Markdown 原文（空字符串表示未发布）
    #[serde(default)]
    pub announcement_markdown: String,
    /// 公告最后更新时间（用于并发编辑校验）
    #[serde(default)]
    pub announcement_updated_at: String,
    /// 公告最后编辑者
    #[serde(default)]
    pub announcement_updated_by: String,
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
    /// 玩家统计落盘的串行锁：保证并发持久化按快照顺序写入，不会互相覆盖
    pub playtime_persist: Mutex<()>,
    /// CPU/内存历史采样 (时间戳, cpu%, mem_mb)
    pub metrics: Mutex<VecDeque<(i64, f32, f64)>>,
    /// TPS/MSPT 采样结果
    pub tps: Mutex<Option<Value>>,
    /// TPS 采样代号：启动 / 停止时递增，用于丢弃上一轮的迟到样本
    pub tps_generation: AtomicU64,
    /// 复用的 RCON 连接（TPS 采样与远程命令）；服务端会为每次新建连接打一行日志
    pub rcon: std::sync::Mutex<Option<crate::rcon::Session>>,
    /// 控制台内存保留的最大日志行数（面板设置可调）
    pub log_limit: std::sync::atomic::AtomicUsize,
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
            playtime_persist: Mutex::new(()),
            metrics: Mutex::new(VecDeque::new()),
            tps: Mutex::new(None),
            tps_generation: AtomicU64::new(0),
            rcon: std::sync::Mutex::new(None),
            log_limit: std::sync::atomic::AtomicUsize::new(DEFAULT_LOG_LIMIT),
            log_buf: Mutex::new(buf),
            log_tx: tx,
            next_seq: AtomicU64::new(next_seq),
        })
    }

    pub async fn persist(&self) -> ApiResult<()> {
        // 持读锁完成序列化与落盘，避免与公告等写者交错产生过期写入
        let meta = self.meta.read().await;
        let json = serde_json::to_string_pretty(&*meta)?;
        self.write_meta_atomic(&json).await
    }

    /// 用给定快照原子落盘。调用方在 meta 写锁内调用，使落盘与内存提交保持一致。
    pub async fn persist_snapshot(&self, meta: &InstanceMeta) -> ApiResult<()> {
        let json = serde_json::to_string_pretty(meta)?;
        self.write_meta_atomic(&json).await
    }

    /// 先写唯一临时文件再原子替换：写入中断不会损坏 instance.json，
    /// 每个写者使用独立临时文件名，避免误删或覆盖彼此的临时文件。
    async fn write_meta_atomic(&self, json: &str) -> ApiResult<()> {
        let tmp = self.dir.join(format!("instance.json.tmp-{}", uuid::Uuid::new_v4()));
        if let Err(e) = tokio::fs::write(&tmp, json).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        if let Err(e) = tokio::fs::rename(&tmp, self.dir.join("instance.json")).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
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
        let status = self.display_status().await.to_string();
        let tps = tps_view(&status, self.tps.lock().await.as_ref(), false);
        let players = self.players.lock().await;
        InstanceSummary {
            id: meta.id,
            name: meta.name,
            created_at: meta.created_at,
            status,
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
            tps: Some(tps),
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

/// TPS 采样超过该秒数未更新后标记为过期
pub const TPS_STALE_SECS: i64 = 30;

/// 将缓存的原始 TPS 采样整理为前端可用的对象。
/// `sanitize_errors` 为 true 时用面向用户的安全提示替换原始 RCON 错误（普通用户接口）。
pub fn tps_view(status: &str, cached: Option<&Value>, sanitize_errors: bool) -> Value {
    if status == "stopped" {
        return json!({ "state": "stopped" });
    }
    let Some(v) = cached else {
        return json!({ "state": "sampling" });
    };
    if v.get("needs_rcon").and_then(|b| b.as_bool()).unwrap_or(false) {
        return json!({ "state": "not_configured", "needs_rcon": true });
    }
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        let msg = if sanitize_errors { "TPS 采样失败，请稍后重试" } else { err };
        return json!({
            "state": "error",
            "error": msg,
            "sampled_at": v.get("sampled_at").cloned().unwrap_or(Value::Null),
        });
    }
    let stale = v
        .get("sampled_at")
        .and_then(|s| s.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() > TPS_STALE_SECS)
        .unwrap_or(false);
    json!({
        "state": if stale { "stale" } else { "ok" },
        "tps": v.get("tps").cloned().unwrap_or(Value::Null),
        "mspt": v.get("mspt").cloned().unwrap_or(Value::Null),
        "sampled_at": v.get("sampled_at").cloned().unwrap_or(Value::Null),
        "stale": stale,
    })
}

/// 发布一次 TPS 采样。先在 tps 锁内重取代号与状态：若采样期间实例已停止或重启
/// （代号变化），丢弃该样本，避免上一轮运行的迟到采样覆盖新一轮状态。
pub async fn publish_tps_sample(rt: &InstanceRuntime, generation: u64, sample: Value) -> bool {
    // 锁顺序：tps → status；reset_tps_cache 同样在 tps 锁内推进代号并清空，
    // 因此代号校验与缓存写入相对于 reset 是原子的。
    let mut guard = rt.tps.lock().await;
    if rt.tps_generation.load(Ordering::SeqCst) != generation {
        return false;
    }
    if *rt.status.lock().await == Status::Stopped {
        return false;
    }
    *guard = Some(sample);
    true
}

/// 清空 TPS 缓存并推进采样代号：停止 / 重启后不再展示上一轮数据。
/// 代号推进与缓存清空在同一 tps 锁内完成，避免与并发发布者交错。
pub async fn reset_tps_cache(rt: &InstanceRuntime) {
    let mut guard = rt.tps.lock().await;
    rt.tps_generation.fetch_add(1, Ordering::SeqCst);
    *guard = None;
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerStat {
    pub name: String,
    pub online: bool,
    pub total_secs: u64,
    pub current_session_secs: u64,
    pub sessions: u32,
}

/// 合并已结算时长与进行中的会话，生成一致的玩家时长快照。
/// 锁顺序固定为 playtime → open_sessions → players，避免与结算路径互锁。
pub async fn playtime_snapshot(rt: &InstanceRuntime) -> Vec<PlayerStat> {
    let now = chrono::Utc::now();
    let pt = rt.playtime.lock().await;
    let open = rt.open_sessions.lock().await;
    let online: std::collections::HashSet<String> = rt.players.lock().await.iter().cloned().collect();
    let mut map: std::collections::BTreeMap<String, PlayerStat> = std::collections::BTreeMap::new();
    for (name, (secs, sessions)) in pt.iter() {
        map.insert(
            name.clone(),
            PlayerStat {
                name: name.clone(),
                online: online.contains(name),
                total_secs: *secs,
                current_session_secs: 0,
                sessions: *sessions,
            },
        );
    }
    for (name, start) in open.iter() {
        let cur = (now - *start).num_seconds().max(0) as u64;
        let is_online = online.contains(name);
        let e = map.entry(name.clone()).or_insert_with(|| PlayerStat {
            name: name.clone(),
            online: is_online,
            total_secs: 0,
            current_session_secs: 0,
            sessions: 0,
        });
        e.online = is_online;
        e.total_secs = e.total_secs.saturating_add(cur);
        e.current_session_secs = cur;
        e.sessions = e.sessions.saturating_add(1);
    }
    let mut list: Vec<PlayerStat> = map.into_values().collect();
    list.sort_by(|a, b| b.total_secs.cmp(&a.total_secs).then_with(|| a.name.cmp(&b.name)));
    list
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

/// 控制台内存保留日志行的默认上限
pub const DEFAULT_LOG_LIMIT: usize = 5000;

pub fn scan_instances(dir: &Path, log_limit: usize) -> HashMap<String, Arc<InstanceRuntime>> {
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
                let rt = InstanceRuntime::new(meta, p);
                rt.log_limit
                    .store(log_limit.max(1), std::sync::atomic::Ordering::Relaxed);
                map.insert(id, rt);
            }
            None => tracing::warn!("instance.json 解析失败: {}", meta_path.display()),
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_rt(tag: &str) -> (Arc<InstanceRuntime>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mcspr-inst-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (InstanceRuntime::new(Default::default(), dir.clone()), dir)
    }

    #[tokio::test]
    async fn old_player_stats_are_restored() {
        let dir = std::env::temp_dir().join(format!("mcspr-stats-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("player-stats.json"), r#"{"Steve":[120,2]}"#).unwrap();
        let rt = InstanceRuntime::new(Default::default(), dir.clone());
        let pt = rt.playtime.lock().await;
        assert_eq!(pt.get("Steve").copied(), Some((120, 2)));
        drop(pt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_includes_first_active_session() {
        let (rt, dir) = temp_rt("snapshot");
        rt.players.lock().await.push("Alex".into());
        rt.open_sessions.lock().await.insert("Alex".into(), chrono::Utc::now());
        let stats = playtime_snapshot(&rt).await;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "Alex");
        assert!(stats[0].online);
        assert_eq!(stats[0].sessions, 1);
        assert_eq!(stats[0].total_secs, stats[0].current_session_secs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_marks_offline_player_without_current_session() {
        let (rt, dir) = temp_rt("offline");
        rt.playtime.lock().await.insert("Herobrine".into(), (300, 4));
        let stats = playtime_snapshot(&rt).await;
        assert_eq!(stats.len(), 1);
        assert!(!stats[0].online);
        assert_eq!(stats[0].total_secs, 300);
        assert_eq!(stats[0].current_session_secs, 0);
        assert_eq!(stats[0].sessions, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tps_view_exposes_states_and_sanitizes_errors() {
        assert_eq!(tps_view("stopped", None, false)["state"], "stopped");
        assert_eq!(tps_view("running", None, false)["state"], "sampling");
        let needs = json!({"needs_rcon": true});
        assert_eq!(tps_view("running", Some(&needs), true)["state"], "not_configured");
        let err = json!({"error": "connect 127.0.0.1:25575 failed"});
        let admin = tps_view("running", Some(&err), false);
        assert_eq!(admin["error"], "connect 127.0.0.1:25575 failed");
        let user = tps_view("running", Some(&err), true);
        assert_eq!(user["error"], "TPS 采样失败，请稍后重试");
        let fresh = json!({"tps": 19.9, "mspt": 1.2, "sampled_at": chrono::Utc::now().to_rfc3339()});
        let ok = tps_view("running", Some(&fresh), true);
        assert_eq!(ok["state"], "ok");
        assert_eq!(ok["stale"], false);
        assert_eq!(ok["tps"], 19.9);
        let old = json!({"tps": 5.0, "sampled_at": (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339()});
        assert_eq!(tps_view("running", Some(&old), true)["state"], "stale");
    }

    #[tokio::test]
    async fn stop_clears_tps_and_late_sample_is_discarded() {
        let (rt, dir) = temp_rt("tps");
        *rt.status.lock().await = Status::Running;
        let generation = rt.tps_generation.load(Ordering::SeqCst);
        assert!(publish_tps_sample(&rt, generation, json!({"tps": 20.0})).await);
        assert!(rt.tps.lock().await.is_some());
        // 模拟停止 / 重启：推进代号并清空缓存
        reset_tps_cache(&rt).await;
        assert!(!publish_tps_sample(&rt, generation, json!({"tps": 20.0})).await);
        assert!(rt.tps.lock().await.is_none());

        *rt.status.lock().await = Status::Stopped;
        let gen2 = rt.tps_generation.load(Ordering::SeqCst);
        assert!(!publish_tps_sample(&rt, gen2, json!({"tps": 20.0})).await);
        assert!(rt.tps.lock().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn blocked_publisher_of_old_generation_is_rejected_after_reset() {
        let (rt, dir) = temp_rt("tps-race");
        *rt.status.lock().await = Status::Running;
        let old_generation = rt.tps_generation.load(Ordering::SeqCst);

        // 占住 tps 锁：reset 与迟到发布者都在等待同一把锁
        let guard = rt.tps.lock().await;

        let rt_reset = rt.clone();
        let reset = tokio::spawn(async move { reset_tps_cache(&rt_reset).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let rt_publish = rt.clone();
        let publish = tokio::spawn(async move {
            publish_tps_sample(&rt_publish, old_generation, json!({"tps": 20.0})).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        drop(guard);
        reset.await.unwrap();
        // reset 先获得锁并推进代号，迟到发布者拿到锁后必须被拒绝
        assert!(!publish.await.unwrap());
        assert!(rt.tps.lock().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
