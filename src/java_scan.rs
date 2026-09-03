//! 本机 Java 扫描：遍历常见安装位置（含启动器自带 JRE），并发探测版本

use crate::util::no_window;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct JavaInfo {
    pub path: String,
    pub version: String,
    pub major: u32,
    pub source: String,
}

fn exe() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// 在 base 下的每个子目录中查找 bin/java
fn collect_dirs(base: &Path, out: &mut Vec<(PathBuf, &'static str)>, source: &'static str) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for e in entries.flatten() {
        let j = e.path().join("bin").join(exe());
        if j.exists() {
            out.push((j, source));
        }
    }
}

pub async fn scan() -> Vec<JavaInfo> {
    let mut found: Vec<(PathBuf, &'static str)> = Vec::new();

    for root in ["C:\\Program Files", "C:\\Program Files (x86)"] {
        let root = Path::new(root);
        collect_dirs(&root.join("Java"), &mut found, "Oracle Java");
        collect_dirs(&root.join("Eclipse Adoptium"), &mut found, "Eclipse Adoptium");
        collect_dirs(&root.join("Microsoft"), &mut found, "Microsoft JDK");
        collect_dirs(&root.join("Zulu"), &mut found, "Azul Zulu");
        collect_dirs(&root.join("Amazon Corretto"), &mut found, "Amazon Corretto");
        collect_dirs(&root.join("BellSoft"), &mut found, "BellSoft Liberica");
        collect_dirs(&root.join("Semeru"), &mut found, "IBM Semeru");
        collect_dirs(&root.join("Java Development Kit"), &mut found, "Oracle JDK");
    }
    // 启动器自带 JRE
    if let Ok(appdata) = std::env::var("APPDATA") {
        let appdata = Path::new(&appdata);
        collect_dirs(&appdata.join("PrismLauncher").join("java"), &mut found, "Prism Launcher");
        collect_dirs(&appdata.join("multimc").join("java"), &mut found, "MultiMC");
        collect_dirs(&appdata.join(".minecraft").join("runtime"), &mut found, "官方启动器");
    }
    // IntelliJ 下载的 JDK
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        collect_dirs(&Path::new(&home).join(".jdks"), &mut found, "IntelliJ .jdks");
    }
    // Linux/macOS 常见位置
    #[cfg(unix)]
    collect_dirs(Path::new("/usr/lib/jvm"), &mut found, "系统 JVM");
    // JAVA_HOME
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = Path::new(&jh).join("bin").join(exe());
        if p.exists() {
            found.push((p, "JAVA_HOME"));
        }
    }
    // PATH 中的默认 java
    found.push((PathBuf::from("java"), "PATH（当前默认）"));

    // 按真实路径去重后并发探测版本
    let mut seen = HashSet::new();
    let mut tasks = Vec::new();
    for (path, source) in found {
        let key = std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        if !seen.insert(key) {
            continue;
        }
        tasks.push(tokio::spawn(probe(path, source)));
    }
    let mut out = Vec::new();
    for t in futures_util::future::join_all(tasks).await {
        if let Ok(Some(info)) = t {
            out.push(info);
        }
    }
    out.sort_by(|a, b| b.major.cmp(&a.major).then(a.path.cmp(&b.path)));
    out
}

/// 读取上次扫描的持久化结果（data/javas.json）
pub fn load_cached(dir: &Path) -> Vec<JavaInfo> {
    std::fs::read_to_string(dir.join("javas.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<JavaInfo>>(&s).ok())
        .unwrap_or_default()
}

pub fn cached_file_exists(dir: &Path) -> bool {
    dir.join("javas.json").exists()
}

pub fn save_cached(dir: &Path, list: &[JavaInfo]) -> std::io::Result<()> {
    std::fs::write(dir.join("javas.json"), serde_json::to_string_pretty(list)?)
}

async fn probe(path: PathBuf, source: &'static str) -> Option<JavaInfo> {
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("-version").stdin(std::process::Stdio::null());
    no_window(&mut cmd);
    let out = cmd.output().await.ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let (version, major) = parse_version(&text);
    if major == 0 {
        return None;
    }
    Some(JavaInfo {
        path: path.to_string_lossy().to_string(),
        version,
        major,
        source: source.to_string(),
    })
}

/// 解析 `java -version` 输出，如 openjdk version "21.0.3" → (整行, 21)；Java 8 返回 8
fn parse_version(text: &str) -> (String, u32) {
    let line = text
        .lines()
        .find(|l| l.contains("version"))
        .unwrap_or("")
        .trim()
        .to_string();
    let raw = line.split('"').nth(1).unwrap_or("");
    let parts: Vec<&str> = raw.split('.').collect();
    let major = match parts.first().and_then(|s| s.parse::<u32>().ok()) {
        Some(1) => parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(8),
        Some(m) => m,
        None => 0,
    };
    (line, major)
}
