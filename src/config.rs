use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 可调阈值（重启风暴熔断 / 磁盘告警）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thresholds {
    /// 崩溃统计窗口（秒）
    #[serde(default = "d_crash_window")]
    pub crash_window_secs: u64,
    /// 窗口内崩溃次数达到该值即停止自动重启
    #[serde(default = "d_crash_max")]
    pub crash_max: u32,
    /// 自动重启延迟（秒）
    #[serde(default = "d_restart_delay")]
    pub restart_delay_secs: u64,
    /// 磁盘告警水位（已用百分比）
    #[serde(default = "d_disk_warn")]
    pub disk_warn_percent: u32,
}
fn d_crash_window() -> u64 { 600 }
fn d_crash_max() -> u32 { 3 }
fn d_restart_delay() -> u64 { 5 }
fn d_disk_warn() -> u32 { 90 }
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            crash_window_secs: d_crash_window(),
            crash_max: d_crash_max(),
            restart_delay_secs: d_restart_delay(),
            disk_warn_percent: d_disk_warn(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelConfig {
    pub listen: String,
    pub data_dir: String,
    #[serde(default)]
    pub token: String,
    /// CurseForge API Key（用于模组搜索下载；留空则仅支持 Modrinth）
    #[serde(default)]
    pub curseforge_api_key: String,
    /// 备份保留：最多份数（超过后清理最旧的）
    #[serde(default = "d_backup_keep")]
    pub backup_keep: u32,
    /// 备份保留：最长天数
    #[serde(default = "d_backup_days")]
    pub backup_keep_days: u64,
    #[serde(default)]
    pub thresholds: Thresholds,
}
fn d_backup_keep() -> u32 { 10 }

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            data_dir: "data".into(),
            token: String::new(),
            curseforge_api_key: String::new(),
            backup_keep: d_backup_keep(),
            backup_keep_days: d_backup_days(),
            thresholds: Thresholds::default(),
        }
    }
}
fn d_backup_days() -> u64 { 30 }

pub fn config_path() -> PathBuf {
    PathBuf::from("config.toml")
}

pub fn load_or_create() -> anyhow::Result<PanelConfig> {
    let path = config_path();
    if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        let cfg: PanelConfig = toml::from_str(&s)?;
        return Ok(cfg);
    }
    let cfg = PanelConfig::default();
    save(&cfg)?;
    Ok(cfg)
}

pub fn save(cfg: &PanelConfig) -> anyhow::Result<()> {
    std::fs::write(config_path(), toml::to_string_pretty(cfg)?)?;
    Ok(())
}

impl PanelConfig {
    pub fn instances_dir(&self) -> PathBuf {
        Path::new(&self.data_dir).join("instances")
    }
}
