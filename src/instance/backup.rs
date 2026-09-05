//! 实例备份：tar.gz 全量备份 / 恢复 / 下载 / 恢复前预览 / 保留策略

use crate::instance::{InstanceRuntime, Status};
use crate::state::AppState;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub size: u64,
    pub created: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestorePreview {
    pub files: u64,
    pub total_size: u64,
    pub top_entries: Vec<String>,
    pub overwrite: Vec<String>,
}

pub async fn backups_dir(state: &AppState, id: &str) -> PathBuf {
    Path::new(&state.config.read().await.data_dir)
        .join("backups")
        .join(id)
}

pub fn list(dir: &Path) -> Vec<BackupInfo> {
    let dir = dir.to_path_buf();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".tar.gz") {
                continue;
            }
            let md = e.metadata().ok();
            let created = md
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default();
            out.push(BackupInfo {
                name,
                size: md.map(|m| m.len()).unwrap_or(0),
                created,
            });
        }
    }
    out.sort_by(|a, b| b.created.cmp(&a.created));
    out
}

/// 备份前如果服务器在运行，发送 save-all 让世界落盘
async fn save_all_if_running(rt: &std::sync::Arc<InstanceRuntime>) {
    if *rt.status.lock().await == Status::Running {
        let _ = crate::instance::process::send_command(rt, "save-all").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub async fn create(state: &AppState, rt: &std::sync::Arc<InstanceRuntime>) -> Result<String, String> {
    save_all_if_running(rt).await;
    let id = rt.meta.read().await.id.clone();
    let (keep, days) = {
        let c = state.config.read().await;
        (c.backup_keep, c.backup_keep_days)
    };
    let dir = backups_dir(state, &id).await;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = format!(
        "backup-{}.tar.gz",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let path = dir.join(&name);
    let src = rt.dir.clone();

    // 打包在阻塞线程执行（目录可能很大）
    let dst = path.clone();
    tokio::task::spawn_blocking(move || pack_tar_gz(&src, &dst).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())??;

    prune(&dir, keep, days);
    Ok(name)
}

/// 保留策略：最多 backup_keep 份 + 最长 backup_keep_days 天
fn prune(dir: &Path, keep: u32, days: u64) {
    let mut items: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name().to_string_lossy().ends_with(".tar.gz")
                })
                .filter_map(|e| {
                    let md = e.metadata().ok()?;
                    Some((e.path(), md.modified().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| b.1.cmp(&a.1)); // 新的在前
    for (i, (path, modified)) in items.iter().enumerate() {
        let too_many = i as u32 >= keep;
        let too_old = modified
            .elapsed()
            .map(|e| e.as_secs() > days * 86400)
            .unwrap_or(false);
        if too_many || too_old {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn pack_tar_gz(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file = std::fs::File::create(dst)?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    {
        let mut builder = tar::Builder::new(&mut enc);
        // 递归追加实例目录（相对路径，统一 / 分隔）
        append_dir_rec(src, src, &mut builder)?;
        builder.finish()?;
    }
    enc.finish()?;
    Ok(())
}

fn append_dir_rec(
    root: &Path,
    dir: &Path,
    builder: &mut tar::Builder<&mut flate2::write::GzEncoder<std::fs::File>>,
) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let rel = p
            .strip_prefix(root)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?
            .to_string_lossy()
            .replace('\\', "/");
        let md = e.metadata()?;
        if md.is_dir() {
            builder.append_dir(&rel, &p)?;
            append_dir_rec(root, &p, builder)?;
        } else {
            builder.append_path_with_name(&p, &rel)?;
        }
    }
    Ok(())
}

fn safe_name(name: &str) -> Result<String, String> {
    if name.contains("..") || name.starts_with('/') || name.contains(':') || name.is_empty() {
        return Err("备份文件名不合法".into());
    }
    if !name.ends_with(".tar.gz") {
        return Err("备份文件名不合法".into());
    }
    Ok(name.to_string())
}

fn backup_path(backups_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = safe_name(name)?;
    Ok(backups_dir.join(name))
}

pub fn backup_file(backups_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let p = backup_path(backups_dir, name)?;
    if !p.exists() {
        return Err("备份不存在".into());
    }
    Ok(p)
}

pub fn delete(backups_dir: &Path, name: &str) -> Result<(), String> {
    let p = backup_path(backups_dir, name)?;
    std::fs::remove_file(p).map_err(|e| format!("删除失败: {e}"))
}

/// 恢复前预览：列出包内文件数、顶层条目与会被覆盖的现有路径
pub fn preview(backups_dir: &Path, instance_dir: &Path, name: &str) -> Result<RestorePreview, String> {
    let p = backup_path(backups_dir, name)?;
    let file = std::fs::File::open(&p).map_err(|e| e.to_string())?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(dec);
    let mut files: u64 = 0;
    let mut total: u64 = 0;
    let mut top = std::collections::BTreeSet::new();
    let mut overwrite = Vec::new();
    for e in archive.entries().map_err(|e| format!("读取备份失败: {e}"))? {
        let e = e.map_err(|e| format!("读取备份失败: {e}"))?;
        let name = e
            .path()
            .map_err(|e| format!("读取备份失败: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        if name.contains("..") {
            return Err("备份包含不安全的路径，已拒绝恢复".into());
        }
        files += 1;
        total += e.size();
        let top_entry = name.split('/').next().unwrap_or("").to_string();
        if !top_entry.is_empty() {
            top.insert(top_entry.clone());
        }
        if instance_dir.join(&name).exists() && overwrite.len() < 50 {
            overwrite.push(name);
        }
    }
    Ok(RestorePreview {
        files,
        total_size: total,
        top_entries: top.into_iter().collect(),
        overwrite,
    })
}

/// 恢复备份（要求实例停止）：解包覆盖实例目录
pub fn restore(instance_dir: &Path, backup: &Path) -> Result<(), String> {
    const MAX_TOTAL: u64 = 32 * 1024 * 1024 * 1024; // 32 GB
    let file = std::fs::File::open(backup).map_err(|e| e.to_string())?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(dec);
    archive.set_overwrite(true);
    let mut total: u64 = 0;
    let mut count: u64 = 0;
    for e in archive.entries().map_err(|e| format!("读取备份失败: {e}"))? {
        let mut e = e.map_err(|e| format!("读取备份失败: {e}"))?;
        let rel = e
            .path()
            .map_err(|e| format!("读取备份失败: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        if rel.contains("..") || Path::new(&rel).is_absolute() {
            return Err("备份包含不安全的路径，已拒绝恢复".into());
        }
        let dest = instance_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if e.header().entry_type().is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            continue;
        }
        let size = e.size();
        total += size;
        count += 1;
        if total > MAX_TOTAL {
            return Err("备份解包总量超过 32GB 上限，已中止".into());
        }
        let mut out = std::fs::File::create(&dest).map_err(|err| format!("写入 {rel} 失败: {err}"))?;
        std::io::copy(&mut e, &mut out).map_err(|err| format!("写入 {rel} 失败: {err}"))?;
    }
    tracing::info!("备份恢复完成：{count} 个文件，{total} 字节");
    Ok(())
}
