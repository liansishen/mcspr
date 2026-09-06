//! 官方服务端版本清单与下载（Mojang 官方源，失败自动切换 BMCLAPI 镜像）

use crate::jobs::{finish_job, log_job, set_progress};
use crate::state::AppState;
use futures_util::StreamExt;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::time::Duration;

const OFFICIAL_META: &str = "https://piston-meta.mojang.com";
const OFFICIAL_DATA: &str = "https://piston-data.mojang.com";
const MIRROR: &str = "https://bmclapi2.bangbang93.com";

/// 生成同一资源的官方源 / 镜像源地址列表
fn url_variants(url: &str) -> Vec<String> {
    let mut out = vec![url.to_string()];
    for host in [OFFICIAL_META, OFFICIAL_DATA, MIRROR] {
        if let Some(rest) = url.strip_prefix(host) {
            let other = if host == MIRROR { OFFICIAL_META } else { MIRROR };
            out.push(format!("{other}{rest}"));
            break;
        }
    }
    out
}

/// 版本清单（缓存 10 分钟）
pub async fn fetch_manifest(state: &AppState) -> Result<Value, String> {
    {
        let cache = state.mc_versions.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, v)) = cache.as_ref() {
            if at.elapsed() < Duration::from_secs(600) {
                return Ok(v.clone());
            }
        }
    }
    let v = fetch_json(
        &state.http,
        "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
    )
    .await?;
    *state.mc_versions.lock().unwrap_or_else(|p| p.into_inner()) = Some((std::time::Instant::now(), v.clone()));
    Ok(v)
}

pub async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let mut last_err = String::new();
    for u in url_variants(url) {
        match http.get(&u).timeout(Duration::from_secs(20)).send().await {
            Ok(resp) if resp.status().is_success() => {
                return resp
                    .json::<Value>()
                    .await
                    .map_err(|e| format!("解析 JSON 失败: {e}"));
            }
            Ok(resp) => last_err = format!("HTTP {}", resp.status()),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("无法访问 {url}：{last_err}（官方源与镜像均不可达）"))
}

/// 后台任务：为实例下载指定版本的官方服务端并配置为主程序
pub async fn download_server(state: &AppState, job_id: &str, instance_id: &str, version: &str) {
    let result = download_server_inner(state, job_id, instance_id, version).await;
    match result {
        Ok(()) => finish_job(state, job_id, None, Some(instance_id.to_string())),
        Err(e) => finish_job(state, job_id, Some(e), Some(instance_id.to_string())),
    }
}

async fn download_server_inner(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    version: &str,
) -> Result<(), String> {
    let rt = crate::instance::get_instance(state, instance_id)
        .await
        .map_err(|e| e.to_string())?;

    log_job(state, job_id, format!("获取版本清单（{version}）…"));
    let manifest = fetch_manifest(state).await?;
    let entry_url = manifest
        .get("versions")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find(|e| e.get("id").and_then(|x| x.as_str()) == Some(version))
        })
        .and_then(|e| e.get("url").and_then(|x| x.as_str()))
        .ok_or_else(|| format!("版本清单中未找到 {version}"))?
        .to_string();

    log_job(state, job_id, "获取版本详情…");
    let vjson = fetch_json(&state.http, &entry_url).await?;
    let server = vjson
        .pointer("/downloads/server")
        .ok_or("该版本没有官方服务端下载（可能是旧版或仅客户端版本）")?;
    let url = server
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or("下载地址缺失")?
        .to_string();
    let size = server.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
    let expected_sha1 = server
        .get("sha1")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    log_job(
        state,
        job_id,
        format!("开始下载服务端 jar（{:.1} MB）…", size as f64 / 1048576.0),
    );

    let dest = rt.dir.join("server.jar.part");
    let mut hasher = Sha1::new();
    let downloaded = download_file(state, job_id, &url, &dest, size, &mut hasher).await?;

    log_job(state, job_id, "校验 SHA1…");
    let hash = format!("{:x}", hasher.finalize());
    if !expected_sha1.is_empty() && hash != expected_sha1 {
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(format!("SHA1 校验失败（预期 {expected_sha1}，实际 {hash}），已删除损坏文件"));
    }

    tokio::fs::rename(&dest, rt.dir.join("server.jar"))
        .await
        .map_err(|e| format!("保存 server.jar 失败: {e}"))?;

    {
        let mut meta = rt.meta.write().await;
        meta.jar = Some("server.jar".into());
    }
    rt.persist().await.map_err(|e| e.to_string())?;

    log_job(
        state,
        job_id,
        format!("✅ 下载完成并通过校验（{downloaded} 字节），已配置为主程序 server.jar"),
    );
    Ok(())
}

async fn download_file(
    state: &AppState,
    job_id: &str,
    url: &str,
    dest: &std::path::Path,
    expected_size: u64,
    hasher: &mut Sha1,
) -> Result<u64, String> {
    let mut last_err = String::new();
    for u in url_variants(url) {
        let resp = match state.http.get(&u).timeout(Duration::from_secs(30)).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                last_err = format!("HTTP {}", r.status());
                continue;
            }
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        let total = resp.content_length().unwrap_or(expected_size);
        let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        let mut last_pct: i32 = -1;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("下载中断: {e}"))?;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            hasher.update(&chunk);
            downloaded += chunk.len() as u64;
            if total > 0 {
                let pct = ((downloaded * 100 / total) as i32).min(99);
                if pct != last_pct {
                    set_progress(state, job_id, pct as u8);
                    last_pct = pct;
                }
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        set_progress(state, job_id, 100);
        return Ok(downloaded);
    }
    Err(format!("下载失败（官方源与镜像均不可达）: {last_err}"))
}
