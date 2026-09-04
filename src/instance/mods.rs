use super::InstanceRuntime;
use crate::error::{ApiError, ApiResult};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
pub struct ModInfo {
    pub file: String,
    pub display_name: String,
    pub version: String,
    pub loader: String,
    pub mc_version: String,
    pub authors: String,
    pub description: String,
    pub size: u64,
    pub enabled: bool,
}

pub const PLUGIN_LOADERS: &[&str] = &[
    "paper",
    "purpur",
    "folia",
    "velocity",
    "waterfall",
    "bungeecord",
];

pub fn is_plugin_loader(loader: &str) -> bool {
    PLUGIN_LOADERS.contains(&loader)
}

/// 实例的插件/模组目录：插件服为 plugins/，其余为 mods/
pub async fn mods_dir(rt: &InstanceRuntime) -> PathBuf {
    let loader = rt.meta.read().await.mod_loader.clone().unwrap_or_default();
    rt.dir.join(if is_plugin_loader(&loader) { "plugins" } else { "mods" })
}

pub async fn list(rt: &Arc<InstanceRuntime>) -> ApiResult<Vec<ModInfo>> {
    let dir = mods_dir(rt).await;
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(&dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        let enabled = fname.ends_with(".jar");
        if !enabled && !fname.ends_with(".jar.disabled") {
            continue;
        }
        let info = tokio::task::spawn_blocking(move || parse_jar(&path))
            .await
            .unwrap_or_else(|_| ModInfo {
                file: fname.clone(),
                display_name: fname.clone(),
                version: "-".into(),
                loader: "未知".into(),
                mc_version: String::new(),
                authors: String::new(),
                description: String::new(),
                size: 0,
                enabled,
            });
        out.push(info);
    }
    out.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    Ok(out)
}

fn clean_file_name(name: &str) -> ApiResult<String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ApiError::bad_request("非法文件名"));
    }
    Ok(name.to_string())
}

pub async fn toggle(rt: &Arc<InstanceRuntime>, file: &str) -> ApiResult<String> {
    let name = clean_file_name(file)?;
    let dir = mods_dir(rt).await;
    let (from, to) = if name.ends_with(".disabled") {
        (name.clone(), name.trim_end_matches(".disabled").to_string())
    } else {
        (name.clone(), format!("{name}.disabled"))
    };
    tokio::fs::rename(dir.join(&from), dir.join(&to)).await?;
    Ok(to)
}

pub async fn delete(rt: &Arc<InstanceRuntime>, file: &str) -> ApiResult<()> {
    let name = clean_file_name(file)?;
    tokio::fs::remove_file(mods_dir(rt).await.join(name)).await?;
    Ok(())
}

type JarArchive = zip::ZipArchive<std::io::BufReader<std::fs::File>>;

/// 解析 jar 内的模组元数据（Fabric / Quilt / Forge / NeoForge）
pub fn parse_jar(path: &Path) -> ModInfo {
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut info = ModInfo {
        file: file_name.clone(),
        display_name: file_name
            .trim_end_matches(".disabled")
            .trim_end_matches(".jar")
            .to_string(),
        version: "-".into(),
        loader: "未知".into(),
        mc_version: String::new(),
        authors: String::new(),
        description: String::new(),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        enabled: !file_name.ends_with(".disabled"),
    };

    let opened = std::fs::File::open(path)
        .ok()
        .and_then(|file| zip::ZipArchive::new(std::io::BufReader::new(file)).ok());
    if let Some(mut zip) = opened {
        parse_jar_meta(&mut zip, &mut info);
        // NeoForge/Forge 的 mods.toml 常用 Maven 占位符（如 ${file.jarVersion}），
        // 运行时由加载器用清单里的 Implementation-Version 替换，这里做同样的回退
        if info.version.contains("${") {
            if let Some(v) = manifest_version(&mut zip) {
                info.version = v;
            }
        }
    }
    info
}

fn manifest_version(zip: &mut JarArchive) -> Option<String> {
    let s = read_entry(zip, "META-INF/MANIFEST.MF").ok()?;
    let line = s
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("implementation-version:"))?;
    let v = line.split_once(':')?.1.trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

fn parse_jar_meta(zip: &mut JarArchive, info: &mut ModInfo) {
    // Fabric
    if let Ok(s) = read_entry(zip, "fabric.mod.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            let name = v
                .get("name")
                .and_then(json_str)
                .filter(|s| !s.is_empty())
                .or_else(|| v.get("id").and_then(json_str));
            if let Some(n) = name {
                info.display_name = n;
            }
            info.version = v.get("version").and_then(json_str).unwrap_or_else(|| "-".into());
            info.loader = "Fabric".into();
            info.description = v.get("description").and_then(json_str).unwrap_or_default();
            info.authors = fmt_authors(v.get("authors"));
            info.mc_version = v.pointer("/depends/minecraft").and_then(json_str).unwrap_or_default();
            return;
        }
    }

    // Quilt
    if let Ok(s) = read_entry(zip, "quilt.mod.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            let loader = &v["quilt_loader"];
            if let Some(n) = loader
                .pointer("/metadata/name")
                .and_then(json_str)
                .or_else(|| loader.get("id").and_then(json_str))
            {
                info.display_name = n;
            }
            info.version = loader
                .get("version")
                .and_then(json_str)
                .unwrap_or_else(|| "-".into());
            info.loader = "Quilt".into();
            info.description = loader.pointer("/metadata/description").and_then(json_str).unwrap_or_default();
            info.mc_version = loader.pointer("/depends/minecraft").and_then(json_str).unwrap_or_default();
            return;
        }
    }

    // NeoForge / Forge
    for (entry_name, loader) in [
        ("META-INF/neoforge.mods.toml", "NeoForge"),
        ("META-INF/mods.toml", "Forge"),
    ] {
        if let Ok(s) = read_entry(zip, entry_name) {
            if let Ok(v) = toml::from_str::<toml::Value>(&s) {
                let m = v.get("mods").and_then(|m| m.as_array()).and_then(|a| a.first());
                if let Some(m) = m {
                    if let Some(n) = m
                        .get("displayName")
                        .and_then(toml_str)
                        .or_else(|| m.get("modId").and_then(toml_str))
                    {
                        info.display_name = n;
                    }
                    info.version = m.get("version").and_then(toml_str).unwrap_or_else(|| "-".into());
                    info.description = m.get("description").and_then(toml_str).unwrap_or_default();
                    info.authors = m.get("authors").and_then(toml_str).unwrap_or_default();
                }
                info.loader = loader.into();
                info.mc_version = mc_from_toml_deps(&v).unwrap_or_default();
                return;
            }
            info.loader = loader.into();
            return;
        }
    }
}

fn read_entry(
    zip: &mut JarArchive,
    name: &str,
) -> Result<String, std::io::Error> {
    let mut f = zip.by_name(name)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

fn json_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => a.first().and_then(json_str),
        _ => None,
    }
}

fn toml_str(v: &toml::Value) -> Option<String> {
    v.as_str().map(|s| s.to_string())
}

fn fmt_authors(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| x.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn mc_from_toml_deps(v: &toml::Value) -> Option<String> {
    let deps = v.get("dependencies")?;
    for (_k, arr) in deps.as_table()? {
        if let Some(arr) = arr.as_array() {
            for d in arr {
                if d.get("modId").and_then(toml_str).as_deref() == Some("minecraft") {
                    return d
                        .get("versionRange")
                        .or_else(|| d.get("version"))
                        .and_then(toml_str);
                }
            }
        }
    }
    None
}
