use crate::error::{ApiError, ApiResult};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

pub const TEXT_EXTS: &[&str] = &[
    "txt", "json", "json5", "toml", "yml", "yaml", "properties", "cfg", "conf", "ini", "mcmeta",
    "mcfunction", "js", "mjs", "cjs", "md", "csv", "log", "sh", "bat", "cmd", "ps1", "py", "xml",
    "html", "css", "scss", "gradle", "kts", "java", "kt", "sql", "access", "suppress",
];

/// 把客户端传来的相对路径安全地拼到 base 下（拒绝绝对路径和 ..）
pub fn safe_join(base: &Path, rel: &str) -> ApiResult<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ApiError::bad_request("路径不合法"));
    }
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(ApiError::bad_request("路径不合法")),
        }
    }
    Ok(base.join(rel_path))
}

#[derive(Serialize)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub modified: String,
}

pub async fn list_dir(base: &Path, rel: &str) -> ApiResult<Vec<FileEntry>> {
    let dir = safe_join(base, rel)?;
    if !dir.is_dir() {
        return Err(ApiError::not_found("目录不存在"));
    }
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(&dir).await?;
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        let (is_dir, size, modified) = match e.metadata().await {
            Ok(m) => (m.is_dir(), m.len(), m.modified().ok()),
            Err(_) => (false, 0, None),
        };
        out.push(FileEntry {
            name,
            dir: is_dir,
            size,
            modified: fmt_time(modified),
        });
    }
    out.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

fn fmt_time(t: Option<std::time::SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => String::new(),
    }
}

pub fn is_text(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .map(|e| TEXT_EXTS.contains(&e.as_str()))
        .unwrap_or(false)
}

pub struct FileContent {
    pub binary: bool,
    pub content: String,
    pub size: u64,
    pub editable: bool,
}

const EDIT_LIMIT: u64 = 2 * 1024 * 1024;

pub async fn read_file(base: &Path, rel: &str) -> ApiResult<FileContent> {
    let p = safe_join(base, rel)?;
    if p.is_dir() {
        return Err(ApiError::bad_request("这是一个目录"));
    }
    let size = tokio::fs::metadata(&p).await.map(|m| m.len()).unwrap_or(0);
    if is_text(rel) && size <= EDIT_LIMIT {
        let bytes = tokio::fs::read(&p).await?;
        match String::from_utf8(bytes) {
            Ok(s) => Ok(FileContent {
                binary: false,
                content: s,
                size,
                editable: true,
            }),
            Err(_) => Ok(FileContent {
                binary: true,
                content: String::new(),
                size,
                editable: false,
            }),
        }
    } else {
        Ok(FileContent {
            binary: true,
            content: String::new(),
            size,
            editable: false,
        })
    }
}

pub async fn write_file(base: &Path, rel: &str, content: &str) -> ApiResult<()> {
    let p = safe_join(base, rel)?;
    if p.is_dir() {
        return Err(ApiError::bad_request("不能覆盖目录"));
    }
    if !is_text(rel) {
        return Err(ApiError::bad_request("不支持编辑此类型的文件"));
    }
    if let Some(parent) = p.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&p, content).await?;
    Ok(())
}

pub async fn mkdir(base: &Path, rel: &str) -> ApiResult<()> {
    let p = safe_join(base, rel)?;
    tokio::fs::create_dir_all(&p).await?;
    Ok(())
}

pub async fn delete(base: &Path, rel: &str) -> ApiResult<()> {
    let p = safe_join(base, rel)?;
    if !p.exists() {
        return Err(ApiError::not_found("文件或目录不存在"));
    }
    if p.is_dir() {
        tokio::fs::remove_dir_all(&p).await?;
    } else {
        tokio::fs::remove_file(&p).await?;
    }
    Ok(())
}

pub async fn rename(base: &Path, from: &str, to: &str) -> ApiResult<()> {
    let fp = safe_join(base, from)?;
    let tp = safe_join(base, to)?;
    if !fp.exists() {
        return Err(ApiError::not_found("源文件不存在"));
    }
    if tp.exists() {
        return Err(ApiError::bad_request("目标名称已存在"));
    }
    tokio::fs::rename(&fp, &tp).await?;
    Ok(())
}

const JAR_SKIP_DIRS: &[&str] = &["libraries", "mods", "cache", "logs", "versions", ".git"];

/// 递归列出实例目录下可作为服务器主程序的 jar（相对路径，正斜杠）
pub fn list_jars(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(base)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if !p.extension().map(|e| e.eq_ignore_ascii_case("jar")).unwrap_or(false) {
            continue;
        }
        let mut skip = false;
        for comp in p.components() {
            if let Some(c) = comp.as_os_str().to_str() {
                if JAR_SKIP_DIRS.contains(&c) {
                    skip = true;
                }
            }
        }
        if skip {
            continue;
        }
        let rel = p
            .strip_prefix(base)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(rel);
    }
    out.sort();
    out
}

/// 读取文件末尾若干行（用于面板重启后恢复控制台上下文）
pub fn tail_lines(path: &Path, max: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return vec![];
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(256 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return vec![];
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return vec![];
    }
    let lines: Vec<String> = buf.lines().map(|s| s.to_string()).collect();
    let n = lines.len();
    if n <= max {
        lines
    } else {
        lines[n - max..].to_vec()
    }
}
