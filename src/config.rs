use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelConfig {
    pub listen: String,
    pub data_dir: String,
    #[serde(default)]
    pub token: String,
    /// CurseForge API Key（用于模组搜索下载；留空则仅支持 Modrinth）
    #[serde(default)]
    pub curseforge_api_key: String,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            data_dir: "data".into(),
            token: String::new(),
            curseforge_api_key: String::new(),
        }
    }
}

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
