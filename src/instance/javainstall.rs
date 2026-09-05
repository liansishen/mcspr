//! 一键安装 Java 运行时（Adoptium Temurin JRE）

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use futures_util::StreamExt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAJORS: &[u32] = &[25, 21, 17, 8];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledJava {
    pub major: u32,
    pub path: String,
}

async fn data_java_dir(state: &AppState) -> PathBuf {
    Path::new(&state.config.read().await.data_dir).join("java")
}

async fn registry_path(state: &AppState) -> PathBuf {
    data_java_dir(state).await.join("java-installs.json")
}

pub async fn load_registry(state: &AppState) -> Vec<InstalledJava> {
    std::fs::read_to_string(registry_path(state).await)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<InstalledJava>>(&s).ok())
        .unwrap_or_default()
}

async fn save_registry(state: &AppState, list: &[InstalledJava]) -> std::io::Result<()> {
    let dir = data_java_dir(state).await;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(registry_path(state).await, serde_json::to_string_pretty(list)?)
}

/// Adoptium 最新 JRE 下载地址（Windows x64 zip）
async fn fetch_latest_url(state: &AppState, major: u32) -> Result<(String, u64), String> {
    let os = if cfg!(windows) { "windows" } else { "linux" };
    let arch = if cfg!(target_arch = "x86_64") { "x64" } else { "aarch64" };
    let image = if cfg!(windows) { "jre" } else { "jdk" };
    let url = format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?architecture={arch}&image_type={image}&os={os}"
    );
    let v: Value = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("请求 Adoptium API 失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP {e}（该大版本可能不存在 JRE 构建）"))?
        .json()
        .await
        .map_err(|e| format!("解析 JSON 失败: {e}"))?;
    let arr = v.as_array().ok_or("响应格式异常")?;
    let link = arr
        .first()
        .and_then(|e| e.pointer("/binary/package/link"))
        .and_then(|x| x.as_str())
        .ok_or("响应缺少下载地址")?
        .to_string();
    let size = arr
        .first()
        .and_then(|e| e.pointer("/binary/package/size"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as u64;
    Ok((link, size))
}

/// 后台任务：下载并解压指定大版本的 Temurin JRE 到 data/java/
pub async fn install(state: &AppState, job_id: &str, major: u32) {
    let result = install_inner(state, job_id, major).await;
    match result {
        Ok(path) => {
            // 记录到注册表，供 Java 扫描结果合并
            let mut list = load_registry(state).await;
            list.retain(|j| j.major != major);
            list.push(InstalledJava { major, path: path.display().to_string() });
            let _ = save_registry(state, &list);
            crate::jobs::finish_job(state, job_id, None, None);
        }
        Err(e) => crate::jobs::finish_job(state, job_id, Some(e), None),
    }
}

async fn install_inner(state: &AppState, job_id: &str, major: u32) -> Result<PathBuf, String> {
    crate::jobs::log_job(state, job_id, format!("查询 Temurin JRE {major} 最新版本…"));
    let (url, size) = fetch_latest_url(state, major).await?;
    crate::jobs::log_job(state, job_id, format!("开始下载（{:.1} MB）…", size as f64 / 1048576.0));

    let dir = data_java_dir(state).await;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip_path = dir.join(format!("temurin-{major}.zip"));
    let resp = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("下载失败: {e}"))?;
    let total = resp.content_length().unwrap_or(size);
    let mut file = tokio::fs::File::create(&zip_path)
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_pct: i32 = -1;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("下载中断: {e}"))?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if total > 0 {
            let pct = ((downloaded * 100 / total) as i32).min(99);
            if pct != last_pct {
                crate::jobs::set_progress(state, job_id, pct as u8);
                last_pct = pct;
            }
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    crate::jobs::set_progress(state, job_id, 100);
    crate::jobs::log_job(state, job_id, "解压中…");

    // 解压（防路径穿越 + 总量上限）
    let f = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(f))
        .map_err(|e| format!("读取 zip 失败: {e}"))?;
    let dest = dir.join(format!("temurin-{major}"));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut e = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = e.enclosed_name() else { continue };
        let out = dest.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        total += e.size();
        if total > 1024 * 1024 * 1024 {
            return Err("解压总量超过 1GB 上限".into());
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out_f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut e, &mut out_f).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(&zip_path);

    // 定位 java 可执行文件
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let mut found: Option<PathBuf> = None;
    for e in walkdir::WalkDir::new(&dest).max_depth(4).into_iter().flatten() {
        let p = e.path();
        if p.is_file() && p.file_name().map(|n| n == exe).unwrap_or(false) {
            found = Some(p.to_path_buf());
            break;
        }
    }
    let java_path = found.ok_or("解压完成但未找到 java 可执行文件")?;
    crate::jobs::log_job(
        state,
        job_id,
        format!("✅ Temurin JRE {major} 安装完成：{}", java_path.display()),
    );
    Ok(java_path)
}
