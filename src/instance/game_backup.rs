use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;

const MAX_TOTAL: u64 = 32 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;
const RECOVERY_DIR: &str = ".mcspr-recovery";
const PROTECTED: &[&str] = &[
    "mods",
    "config",
    "libraries",
    "logs",
    "backups",
    "instance.json",
    "server.properties",
    "eula.txt",
    "player-stats.json",
    RECOVERY_DIR,
];

#[derive(Debug, Clone, Serialize)]
pub struct Provider {
    pub provider: String,
    pub version: String,
    pub backup_dir: String,
    pub enabled: bool,
    pub command_enabled: bool,
    pub interval_hours: f64,
    pub keep: u32,
    pub need_online_players: bool,
    pub only_claimed: bool,
    #[serde(skip)]
    extra_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub files: u64,
    pub total_size: u64,
    pub roots: Vec<String>,
    pub world: String,
    pub recovery_directory: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreResult {
    pub recovery_directory: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub size: u64,
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| e.to_string())
}

fn no_links(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(format!("不允许符号链接路径：{}", current.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn root(dir: &Path) -> Result<PathBuf, String> {
    no_links(dir)?;
    let dir = fs::canonicalize(dir).map_err(|e| e.to_string())?;
    if !dir.is_dir() {
        return Err("实例目录无效".into());
    }
    Ok(dir)
}

fn relative(value: &str) -> Result<String, String> {
    if value.is_empty() || value.contains(['\\', ':']) || value.chars().any(char::is_control) {
        return Err("路径不合法".into());
    }
    let path = Path::new(value);
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return Err("路径必须位于实例内".into()),
        }
    }
    if parts.is_empty() {
        return Err("路径不能为空或指向实例根目录".into());
    }
    Ok(parts.join("/"))
}

// Forge 配置的标量和列表按所属段读取，忽略注释。
fn section(text: &str, name: &str) -> (BTreeMap<String, String>, BTreeMap<String, Vec<String>>) {
    let mut values = BTreeMap::new();
    let mut lists = BTreeMap::new();
    let mut inside = false;
    let mut list: Option<(String, Vec<String>)> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !inside {
            if line.ends_with('{') && line.trim_end_matches('{').trim().trim_matches('"') == name {
                inside = true;
            }
            continue;
        }
        if let Some((key, entries)) = &mut list {
            if line == ">" {
                lists.insert(key.clone(), entries.clone());
                list = None;
            } else {
                entries.push(line.trim_matches('"').to_string());
            }
            continue;
        }
        if line == "}" {
            break;
        }
        if let Some(key) = line.strip_suffix('<') {
            let key = key
                .trim()
                .split_once(':')
                .map(|(_, k)| k)
                .unwrap_or(key.trim());
            list = Some((key.trim_matches('"').into(), Vec::new()));
        } else if let Some((key, value)) = line.split_once('=') {
            let key = key
                .trim()
                .split_once(':')
                .map(|(_, k)| k)
                .unwrap_or(key.trim());
            values.insert(
                key.trim_matches('"').into(),
                value.trim().trim_matches('"').into(),
            );
        }
    }
    (values, lists)
}

pub fn detect(dir: &Path) -> Result<Option<Provider>, String> {
    let dir = root(dir)?;
    let mods = dir.join("mods");
    if !mods.exists() {
        return Ok(None);
    }
    no_links(&mods)?;
    for entry in fs::read_dir(&mods).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jar")
            || !entry.file_type().map_err(|e| e.to_string())?.is_file()
        {
            continue;
        }
        no_links(&path)?;
        let mut zip = match ZipArchive::new(fs::File::open(path).map_err(|e| e.to_string())?) {
            Ok(zip) => zip,
            Err(_) => continue,
        };
        let mut metadata = match zip.by_name("mcmod.info") {
            Ok(file) => file,
            Err(_) => continue,
        };
        if metadata.size() > 512 * 1024 {
            continue;
        }
        let mut bytes = Vec::new();
        if metadata.read_to_end(&mut bytes).is_err() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let entries = value
            .as_array()
            .or_else(|| value.get("modList").and_then(|v| v.as_array()));
        let Some(info) = entries.and_then(|entries| {
            entries
                .iter()
                .find(|m| m["modid"].as_str() == Some("serverutilities"))
        }) else {
            continue;
        };
        let config_path = dir.join("serverutilities/serverutilities.cfg");
        no_links(&config_path)?;
        let text = fs::read_to_string(&config_path)
            .map_err(|e| format!("已识别 ServerUtilities，读取配置失败：{e}"))?;
        let (settings, lists) = section(&text, "backups");
        let (commands, _) = section(&text, "commands");
        let boolean = |key: &str, default: bool| -> Result<bool, String> {
            match settings.get(key).map(String::as_str) {
                None => Ok(default),
                Some("true") => Ok(true),
                Some("false") => Ok(false),
                _ => Err(format!("ServerUtilities 配置 {key} 无效")),
            }
        };
        let interval_hours: f64 = settings
            .get("backup_timer")
            .map(String::as_str)
            .unwrap_or("0.5")
            .parse()
            .map_err(|_| "备份间隔无效")?;
        if !interval_hours.is_finite() || interval_hours < 0.0 {
            return Err("备份间隔无效".into());
        }
        let keep = settings
            .get("backups_to_keep")
            .map(String::as_str)
            .unwrap_or("12")
            .parse()
            .map_err(|_| "备份保留数量无效")?;
        let provider = Provider {
            provider: "ServerUtilities".into(),
            version: info["version"].as_str().unwrap_or("unknown").into(),
            backup_dir: relative(
                settings
                    .get("backup_folder_path")
                    .map(String::as_str)
                    .unwrap_or("./backups/"),
            )?,
            enabled: boolean("enable_backups", true)?,
            command_enabled: match commands.get("backup").map(String::as_str).unwrap_or("true") {
                "true" => true,
                "false" => false,
                _ => return Err("backup 命令配置无效".into()),
            },
            interval_hours,
            keep,
            need_online_players: boolean("need_online_players", true)?,
            only_claimed: boolean("only_backup_claimed_chunks", false)?,
            extra_patterns: lists
                .get("additional_backup_files")
                .cloned()
                .unwrap_or_else(|| {
                    vec![
                        "saves/NEI/global/**".into(),
                        "saves/NEI/local/$WORLDNAME/**".into(),
                    ]
                }),
        };
        backup_dir(&dir, &provider)?;
        return Ok(Some(provider));
    }
    Ok(None)
}

fn world(dir: &Path) -> Result<String, String> {
    let props = dir.join("server.properties");
    no_links(&props)?;
    let text = match fs::read_to_string(props) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let name = text
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .find(|(k, _)| k.trim() == "level-name")
        .map(|(_, v)| v.trim())
        .unwrap_or("world");
    let name = relative(name)?;
    if name.contains('/') || PROTECTED.contains(&name.as_str()) || name.starts_with('.') {
        return Err("level-name 必须为安全的世界目录名".into());
    }
    Ok(name)
}

pub fn backup_dir(dir: &Path, provider: &Provider) -> Result<PathBuf, String> {
    if provider.provider != "ServerUtilities" {
        return Err("不支持的备份模组".into());
    }
    let dir = root(dir)?;
    let name = relative(&provider.backup_dir)?;
    let top = name.split('/').next().unwrap();
    if (PROTECTED.contains(&top) && top != "backups")
        || top == world(&dir)?
        || top == "serverutilities"
        || top == "visualprospecting"
        || top.starts_with('.')
    {
        return Err("备份目录与实例数据或保护路径冲突".into());
    }
    let path = dir.join(name);
    no_links(&path)?;
    Ok(path)
}

pub fn backup_file(dir: &Path, provider: &Provider, name: &str) -> Result<PathBuf, String> {
    if !name.ends_with(".zip")
        || relative(name)? != name
        || name.contains('/')
        || name.starts_with('.')
    {
        return Err("备份文件名必须为单段 .zip 文件名".into());
    }
    let path = backup_dir(dir, provider)?.join(name);
    no_links(&path)?;
    if !fs::metadata(&path).map_err(|_| "备份不存在")?.is_file() {
        return Err("备份不是普通文件".into());
    }
    Ok(path)
}

pub fn list(dir: &Path, provider: &Provider) -> Result<Vec<BackupInfo>, String> {
    let storage = backup_dir(dir, provider)?;
    let entries = match fs::read_dir(storage) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".zip") || name.starts_with('.') {
            continue;
        }
        let checked = backup_file(dir, provider, &name);
        let problem = match &checked {
            Ok(path) => fs::File::open(path)
                .map_err(|e| e.to_string())
                .and_then(|file| {
                    ZipArchive::new(file)
                        .map(|_| ())
                        .map_err(|e| format!("ZIP 不完整或损坏：{e}"))
                })
                .err(),
            Err(error) => Some(error.clone()),
        };
        let md = entry.metadata().ok();
        let created = md
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                let time: chrono::DateTime<chrono::Local> = t.into();
                time.format("%Y-%m-%d %H:%M:%S").to_string()
            })
            .unwrap_or_default();
        backups.push(BackupInfo {
            name,
            size: md.map(|m| m.len()).unwrap_or(0),
            created,
            problem,
        });
    }
    backups.sort_by(|a, b| b.created.cmp(&a.created).then(b.name.cmp(&a.name)));
    Ok(backups)
}

fn glob(pattern: &str, name: &str) -> Result<bool, String> {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            if chars.peek() == Some(&'*') {
                chars.next();
                regex.push_str(".*");
            } else {
                regex.push_str("[^/]*");
            }
        } else {
            regex.push_str(&regex::escape(&c.to_string()));
        }
    }
    regex.push('$');
    regex::Regex::new(&regex)
        .map(|regex| regex.is_match(name))
        .map_err(|e| e.to_string())
}

fn allowed_extra(provider: &Provider, world: &str, name: &str) -> Result<bool, String> {
    if matches!(
        name,
        "serverutilities/server/ranks.txt" | "serverutilities/server/players.txt"
    ) {
        return Ok(true);
    }
    if provider.extra_patterns.len() > 128 {
        return Err("额外备份路径过多".into());
    }
    for pattern in &provider.extra_patterns {
        let pattern = relative(&pattern.replace("$WORLDNAME", world))?;
        let top = pattern.split('/').next().unwrap();
        if PROTECTED.contains(&top)
            || top == "serverutilities"
            || top.starts_with('.')
            || top.contains('*')
        {
            return Err(format!("不支持恢复配置中的额外路径：{pattern}"));
        }
        if glob(&pattern, name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn inspect(
    dir: &Path,
    provider: &Provider,
    path: &Path,
    stage: Option<&Path>,
) -> Result<Preview, String> {
    let world = world(dir)?;
    let mut archive = ZipArchive::new(fs::File::open(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("无效 ZIP：{e}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err("ZIP 条目数量超限".into());
    }
    let mut seen = BTreeMap::new();
    let mut targets = BTreeSet::new();
    let mut file_paths = Vec::new();
    let mut files = 0;
    let mut total = 0u64;
    let mut has_level = false;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let raw = entry.name().to_string();
        let name = raw.trim_end_matches('/');
        if name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
            || relative(name)? != name
        {
            return Err(format!("ZIP 路径不安全：{raw}"));
        }
        if entry
            .unix_mode()
            .map(|mode| mode & 0o170000)
            .is_some_and(|kind| !matches!(kind, 0 | 0o100000 | 0o040000))
        {
            return Err("ZIP 包含链接或特殊文件".into());
        }
        if seen.insert(name.to_string(), entry.is_dir()).is_some() {
            return Err("ZIP 包含重复条目".into());
        }
        let top = name.split('/').next().unwrap();
        if top != world
            && (PROTECTED.contains(&top)
                || top == provider.backup_dir.split('/').next().unwrap_or("")
                || top.starts_with('.'))
        {
            return Err(format!("ZIP 包含保护路径：{name}"));
        }
        if entry.is_dir() {
            continue;
        }
        if top != world && !allowed_extra(provider, &world, name)? {
            return Err(format!("ZIP 包含未授权数据路径：{name}"));
        }
        files += 1;
        total = total.checked_add(entry.size()).ok_or("ZIP 总大小溢出")?;
        if total > MAX_TOTAL {
            return Err("ZIP 解压总量超过 32GB".into());
        }
        has_level |= name == format!("{world}/level.dat");
        targets.insert(if top == world {
            world.clone()
        } else {
            name.to_string()
        });
        file_paths.push(name.to_string());
        let size = entry.size();
        let copied = if let Some(stage) = stage {
            let destination = stage.join(name);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)
                .map_err(|e| e.to_string())?;
            let copied = std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("ZIP CRC/解压校验失败：{e}"))?;
            out.flush().map_err(|e| e.to_string())?;
            out.sync_all().map_err(|e| e.to_string())?;
            copied
        } else {
            std::io::copy(&mut entry, &mut std::io::sink())
                .map_err(|e| format!("ZIP CRC 校验失败：{e}"))?
        };
        if copied != size {
            return Err("ZIP 文件大小不一致".into());
        }
    }
    if !has_level {
        return Err("ZIP 缺少当前世界的 level.dat".into());
    }
    for (name, is_dir) in &seen {
        let mut ancestor = Path::new(name).parent();
        while let Some(path) = ancestor {
            if seen.get(path.to_string_lossy().as_ref()) == Some(&false) {
                return Err("ZIP 文件与目录冲突".into());
            }
            ancestor = path.parent();
        }
        if *is_dir
            && name.split('/').next() != Some(&world)
            && !file_paths
                .iter()
                .any(|file| file.starts_with(&format!("{name}/")))
        {
            return Err("ZIP 包含范围外的空目录".into());
        }
    }
    Ok(Preview {
        files,
        total_size: total,
        roots: targets.into_iter().collect(),
        world,
        recovery_directory: dir.join(RECOVERY_DIR).to_string_lossy().into_owned(),
    })
}

pub fn preview(dir: &Path, provider: &Provider, name: &str) -> Result<Preview, String> {
    let dir = root(dir)?;
    inspect(&dir, provider, &backup_file(&dir, provider, name)?, None)
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn commit(dir: &Path, stage: &Path, recovery: &Path, targets: &[String]) -> Result<(), String> {
    for target in targets {
        no_links(&dir.join(target))?;
    }
    let mut moved = Vec::new();
    let mut installed = Vec::new();
    let result = (|| -> Result<(), String> {
        for target in targets {
            let destination = dir.join(target);
            no_links(&destination)?;
            if destination.exists() {
                let preserved = recovery.join(target);
                if let Some(parent) = preserved.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::rename(&destination, &preserved).map_err(|e| e.to_string())?;
                moved.push((preserved, destination.clone()));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::rename(stage.join(target), &destination).map_err(|e| e.to_string())?;
            installed.push(destination);
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        for installed in installed.iter().rev() {
            if let Err(e) = remove_path(installed) {
                failures.push(e.to_string());
            }
        }
        for (preserved, destination) in moved.iter().rev() {
            if let Err(e) = fs::rename(preserved, destination) {
                failures.push(e.to_string());
            }
        }
        if failures.is_empty() {
            return Err(format!("还原失败，当前数据已回滚：{error}"));
        }
        return Err(format!(
            "还原失败：{error}；回滚失败：{}；原数据保留于 {}",
            failures.join("；"),
            recovery.display()
        ));
    }
    Ok(())
}

pub fn restore(dir: &Path, provider: &Provider, name: &str) -> Result<RestoreResult, String> {
    let dir = root(dir)?;
    let backup = backup_file(&dir, provider, name)?;
    let nonce = format!(
        "{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4()
    );
    let stage = dir.join(format!(".mcspr-restore-{nonce}"));
    create_private_dir(&stage)?;
    let result = (|| -> Result<RestoreResult, String> {
        let preview = inspect(&dir, provider, &backup, Some(&stage))?;
        let parent = dir.join(RECOVERY_DIR);
        no_links(&parent)?;
        fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let recovery = parent.join(&nonce);
        create_private_dir(&recovery)?;
        fs::write(
            recovery.join("manifest.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "backup": name, "world": preview.world, "paths": preview.roots,
            }))
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        commit(&dir, &stage, &recovery, &preview.roots)?;
        Ok(RestoreResult {
            recovery_directory: recovery.to_string_lossy().into_owned(),
        })
    })();
    let cleanup = fs::remove_dir_all(&stage);
    match (result, cleanup) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(e)) => Err(format!(
            "还原已完成，但清理暂存目录失败：{e}；路径 {}",
            stage.display()
        )),
        (Err(e), _) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::{write::SimpleFileOptions, ZipWriter};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("mcspr-game-backup-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("serverutilities")).unwrap();
            fs::write(root.join("server.properties"), "level-name=World\n").unwrap();
            fs::write(root.join("serverutilities/serverutilities.cfg"), "# backups { fake comment\nbackups {\n S:backup_folder_path=./backups/\n S:backup_timer=1.5\n I:backups_to_keep=7\n B:enable_backups=true\n B:need_online_players=false\n S:additional_backup_files <\n visualprospecting/server/$WORLDNAME_*/**\n journeymap/data/sp/$WORLDNAME/**\n >\n}\ncommands {\n B:backup=false\n}\n").unwrap();
            let fixture = Self(root);
            fixture.jar("su.jar");
            fixture
        }
        fn jar(&self, name: &str) {
            fs::create_dir_all(self.0.join("mods")).unwrap();
            zip_file(
                &self.0.join("mods").join(name),
                &[(
                    "mcmod.info",
                    br#"{"modList":[{"modid":"serverutilities","version":"2.4.14"}]}"#,
                )],
            );
        }
        fn provider(&self) -> Provider {
            detect(&self.0).unwrap().unwrap()
        }
        fn backup(&self, name: &str, entries: &[(&str, &[u8])]) {
            zip_file(&self.0.join("backups").join(name), entries);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn zip_file(path: &Path, entries: &[(&str, &[u8])]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut zip = ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn detects_real_config_path_and_exact_sections() {
        let f = Fixture::new();
        let p = f.provider();
        assert_eq!(p.version, "2.4.14");
        assert_eq!(p.backup_dir, "backups");
        assert_eq!(p.interval_hours, 1.5);
        assert_eq!(p.keep, 7);
        assert!(!p.command_enabled && !p.need_online_players);
        fs::rename(f.0.join("mods/su.jar"), f.0.join("mods/su.jar.disabled")).unwrap();
        assert!(detect(&f.0).unwrap().is_none());
    }

    #[test]
    fn backup_path_is_read_only_and_scoped() {
        let f = Fixture::new();
        let p = f.provider();
        assert!(backup_file(&f.0, &p, "missing.zip").is_err());
        assert!(!f.0.join("backups").exists());
        f.backup("real.zip", &[("World/level.dat", b"old")]);
        let bytes = fs::read(f.0.join("backups/real.zip")).unwrap();
        backup_file(&f.0, &p, "real.zip").unwrap();
        assert_eq!(fs::read(f.0.join("backups/real.zip")).unwrap(), bytes);
        assert!(backup_file(&f.0, &p, "../real.zip").is_err());
        let mut p = p;
        p.backup_dir = "/tmp".into();
        assert!(backup_dir(&f.0, &p).is_err());
        p.backup_dir = "mods/backups".into();
        assert!(backup_dir(&f.0, &p).is_err());
    }

    #[test]
    fn damaged_archive_does_not_hide_valid_backups() {
        let f = Fixture::new();
        let p = f.provider();
        f.backup("valid.zip", &[("World/level.dat", b"old")]);
        fs::write(f.0.join("backups/damaged.zip"), b"broken ZIP").unwrap();
        let backups = list(&f.0, &p).unwrap();
        assert_eq!(backups.len(), 2);
        assert!(backups
            .iter()
            .find(|b| b.name == "valid.zip")
            .unwrap()
            .problem
            .is_none());
        assert!(backups
            .iter()
            .find(|b| b.name == "damaged.zip")
            .unwrap()
            .problem
            .is_some());
    }

    #[test]
    #[cfg(unix)]
    fn staging_is_private_and_failed_restore_leaves_no_staging_directory() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let p = f.provider();
        let private = f.0.join("private");
        create_private_dir(&private).unwrap();
        assert_eq!(
            fs::metadata(private).unwrap().permissions().mode() & 0o777,
            0o700
        );
        f.backup(
            "bad.zip",
            &[("World/level.dat", b"old"), ("../escape", b"bad")],
        );
        assert!(restore(&f.0, &p, "bad.zip").is_err());
        assert!(!fs::read_dir(&f.0).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".mcspr-restore-")));
    }

    #[test]
    fn zip_paths_scopes_and_conflicts_are_rejected() {
        let f = Fixture::new();
        let p = f.provider();
        for bad in [
            "../escape",
            "/abs",
            "C:/windows",
            "World/../escape",
            "World\\escape",
            "mods/new.jar",
            "Other/level.dat",
            "serverutilities/serverutilities.cfg",
            "visualprospecting/server/Other_UUID/a.dat",
        ] {
            f.backup("bad.zip", &[("World/level.dat", b"old"), (bad, b"bad")]);
            assert!(preview(&f.0, &p, "bad.zip").is_err(), "{bad}");
        }
        f.backup("missing.zip", &[("World/file", b"old")]);
        assert!(preview(&f.0, &p, "missing.zip").is_err());
        f.backup(
            "conflict.zip",
            &[
                ("World/level.dat", b"old"),
                ("World/a", b"file"),
                ("World/a/b", b"child"),
            ],
        );
        assert!(preview(&f.0, &p, "conflict.zip").is_err());
    }

    #[test]
    fn preview_and_restore_include_real_uuid_and_preserve_current_data() {
        let f = Fixture::new();
        let p = f.provider();
        let prospect =
            "visualprospecting/server/World_1455ba6c-4b87-4d4d-ba6c-263390d8fc72/DIM0.dat";
        f.backup(
            "old.zip",
            &[
                ("World/level.dat", b"old"),
                ("World/region/a.mca", b"region"),
                ("serverutilities/server/ranks.txt", b"oldrank"),
                (prospect, b"oldvp"),
            ],
        );
        fs::create_dir_all(f.0.join("World")).unwrap();
        fs::write(f.0.join("World/level.dat"), b"new").unwrap();
        fs::write(f.0.join("World/newer"), b"preserve").unwrap();
        fs::write(f.0.join("instance.json"), b"metadata").unwrap();
        fs::create_dir_all(f.0.join("serverutilities/server")).unwrap();
        fs::write(f.0.join("serverutilities/server/ranks.txt"), b"newrank").unwrap();
        fs::write(f.0.join("serverutilities/server/keep.txt"), b"keep").unwrap();
        let config = fs::read(f.0.join("serverutilities/serverutilities.cfg")).unwrap();
        let preview = preview(&f.0, &p, "old.zip").unwrap();
        assert!(preview.roots.contains(&prospect.into()));
        let result = restore(&f.0, &p, "old.zip").unwrap();
        assert_eq!(fs::read(f.0.join("World/level.dat")).unwrap(), b"old");
        assert!(!f.0.join("World/newer").exists());
        assert_eq!(
            fs::read(Path::new(&result.recovery_directory).join("World/newer")).unwrap(),
            b"preserve"
        );
        assert_eq!(
            fs::read(
                Path::new(&result.recovery_directory).join("serverutilities/server/ranks.txt")
            )
            .unwrap(),
            b"newrank"
        );
        assert_eq!(fs::read(f.0.join(prospect)).unwrap(), b"oldvp");
        assert_eq!(
            fs::read(f.0.join("serverutilities/server/keep.txt")).unwrap(),
            b"keep"
        );
        assert_eq!(
            fs::read(f.0.join("serverutilities/serverutilities.cfg")).unwrap(),
            config
        );
        assert_eq!(fs::read(f.0.join("instance.json")).unwrap(), b"metadata");
        assert!(f.0.join("backups/old.zip").exists());
    }

    #[test]
    fn crc_failure_preserves_world_and_commit_failure_rolls_back() {
        let f = Fixture::new();
        let p = f.provider();
        f.backup(
            "crc.zip",
            &[("World/level.dat", b"unique old world marker")],
        );
        let path = f.0.join("backups/crc.zip");
        let mut bytes = fs::read(&path).unwrap();
        let marker = b"unique old world marker";
        let index = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap();
        bytes[index] ^= 1;
        fs::write(path, bytes).unwrap();
        fs::create_dir_all(f.0.join("World")).unwrap();
        fs::write(f.0.join("World/level.dat"), b"current").unwrap();
        assert!(restore(&f.0, &p, "crc.zip").is_err());
        assert_eq!(fs::read(f.0.join("World/level.dat")).unwrap(), b"current");
        let stage = f.0.join("stage");
        let recovery = f.0.join("recovery");
        fs::create_dir_all(stage.join("World")).unwrap();
        fs::create_dir_all(&recovery).unwrap();
        fs::write(stage.join("World/level.dat"), b"restored").unwrap();
        assert!(
            commit(&f.0, &stage, &recovery, &["World".into(), "missing".into()])
                .unwrap_err()
                .contains("已回滚")
        );
        assert_eq!(fs::read(f.0.join("World/level.dat")).unwrap(), b"current");
    }

    #[test]
    #[cfg(unix)]
    fn links_in_zip_and_filesystem_are_rejected() {
        let f = Fixture::new();
        let p = f.provider();
        std::os::unix::fs::symlink("/tmp", f.0.join("linked")).unwrap();
        let mut external = p.clone();
        external.backup_dir = "linked/backups".into();
        assert!(backup_dir(&f.0, &external).is_err());
        f.backup("link.zip", &[("World/level.dat", b"world")]);
        let path = f.0.join("backups/link.zip");
        let mut zip = ZipWriter::new_append(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        )
        .unwrap();
        zip.add_symlink("World/link", "/tmp", SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        assert!(preview(&f.0, &p, "link.zip").is_err());
    }
}
