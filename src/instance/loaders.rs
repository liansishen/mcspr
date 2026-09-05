//! 模组加载器服务端安装：Fabric / Quilt（官方启动器 jar 一键包）、Forge / NeoForge（运行官方安装器）

use crate::instance::files;
use crate::jobs::log_job;
use crate::state::AppState;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

fn http(state: &AppState) -> &reqwest::Client {
    &state.http
}

async fn get_json(state: &AppState, url: &str) -> Result<Value, String> {
    http(state)
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP {e}"))?
        .json::<Value>()
        .await
        .map_err(|e| format!("解析 JSON 失败: {e}"))
}

fn version_key(s: &str) -> Vec<u64> {
    s.split(|c| c == '.' || c == '-')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// 列出某加载器可用的 MC 版本
pub async fn game_versions(state: &AppState, loader: &str) -> Result<Vec<Value>, String> {
    let mut out: Vec<(String, bool)> = Vec::new();
    match loader {
        "fabric" | "quilt" => {
            let base = if loader == "fabric" {
                "https://meta.fabricmc.net/v2/versions/game"
            } else {
                "https://meta.quiltmc.org/v3/versions/game"
            };
            let v = get_json(state, base).await?;
            if let Some(arr) = v.as_array() {
                for e in arr {
                    if let Some(ver) = e.get("version").and_then(|x| x.as_str()) {
                        let stable = e.get("stable").and_then(|x| x.as_bool()).unwrap_or(false);
                        out.push((ver.to_string(), stable));
                    }
                }
            }
        }
        "paper" | "folia" | "waterfall" | "velocity" => {
            // PaperMC Fill API v3：versions 为分组对象，拍平后按版本倒序
            let v = get_json(
                state,
                &format!("https://fill.papermc.io/v3/projects/{loader}"),
            )
            .await?;
            if let Some(groups) = v.get("versions").and_then(|x| x.as_object()) {
                for (_group, arr) in groups {
                    if let Some(list) = arr.as_array() {
                        for e in list {
                            if let Some(ver) = e.as_str() {
                                out.push((ver.to_string(), true));
                            }
                        }
                    }
                }
            }
            out.sort_by(|a, b| version_key(&b.0).cmp(&version_key(&a.0)));
            out.dedup();
        }
        "purpur" => {
            let v = get_json(state, "https://api.purpurmc.org/v2/purpur").await?;
            if let Some(arr) = v.get("versions").and_then(|x| x.as_array()) {
                for e in arr {
                    if let Some(ver) = e.as_str() {
                        out.push((ver.to_string(), true));
                    }
                }
            }
        }
        "forge" => {
            // 用完整 maven-metadata（覆盖所有旧版本）
            let builds = forge_builds(state).await?;
            let mut seen = std::collections::HashSet::new();
            for b in &builds {
                if let Some(mc) = b.split('-').next() {
                    if seen.insert(mc.to_string()) {
                        out.push((mc.to_string(), true));
                    }
                }
            }
        }
        "neoforge" => {
            let v = get_json(
                state,
                "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge",
            )
            .await?;
            if let Some(arr) = v.get("versions").and_then(|x| x.as_array()) {
                for e in arr {
                    if let Some(ver) = e.as_str() {
                        if let Some(mc) = neoforge_mc_version(ver) {
                            if !out.iter().any(|(v, _)| v.as_str() == mc.as_str()) {
                                out.push((mc, true));
                            }
                        }
                    }
                }
            }
        }
        _ => return Err(format!("未知加载器: {loader}")),
    }
    out.sort_by(|a, b| version_key(&b.0).cmp(&version_key(&a.0)));
    Ok(out
        .into_iter()
        .map(|(v, stable)| json!({ "id": v, "stable": stable }))
        .collect())
}

/// NeoForge 版本号 → 对应 MC 版本：
/// - 1.20.1 时代：1.20.1-47.1.3 → 1.20.1（"-" 前完整保留）
/// - 1.x 时代（≤21）：21.4.157 → 1.21.4（去掉前导 "1." 再补回）
/// - 年份制（2026 起，≥22）：26.2.5 → 26.2（与 MC 版本保持一致，MC 已无 "1." 前缀）
fn neoforge_mc_version(ver: &str) -> Option<String> {
    if let Some((mc, _)) = ver.split_once('-') {
        if mc.starts_with("1.") {
            return Some(mc.to_string());
        }
    }
    let parts: Vec<&str> = ver.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let major: u32 = parts[0].parse().ok()?;
    if major == 0 {
        return None;
    }
    if major <= 21 {
        Some(format!("1.{}.{}", parts[0], parts[1]))
    } else {
        Some(format!("{}.{}", parts[0], parts[1]))
    }
}

/// 列出某加载器在指定 MC 版本下的加载器版本
pub async fn loader_versions(
    state: &AppState,
    loader: &str,
    game: &str,
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    match loader {
        "fabric" | "quilt" => {
            let base = if loader == "fabric" {
                "https://meta.fabricmc.net/v2/versions/loader"
            } else {
                "https://meta.quiltmc.org/v3/versions/loader"
            };
            let v = get_json(state, base).await?;
            if let Some(arr) = v.as_array() {
                for e in arr {
                    if let Some(ver) = e.get("version").and_then(|x| x.as_str()) {
                        out.push(ver.to_string());
                    }
                }
            }
            out.truncate(30);
        }
        "paper" | "folia" | "waterfall" => {
            let v = get_json(
                state,
                &format!("https://fill.papermc.io/v3/projects/{loader}/versions/{game}/builds"),
            )
            .await?;
            if let Some(arr) = v.as_array() {
                for b in arr {
                    if let Some(n) = b.get("id").and_then(|x| x.as_i64()) {
                        out.push(n.to_string());
                    }
                }
            }
            // v3 返回新→旧，保持
        }
        "purpur" => {
            let v = get_json(state, &format!("https://api.purpurmc.org/v2/purpur/{game}")).await?;
            if let Some(builds) = v.get("builds").and_then(|x| x.as_object()) {
                if let Some(n) = builds.get("latest").and_then(|x| x.as_str()) {
                    out.push(n.to_string());
                }
                if let Some(arr) = builds.get("all").and_then(|x| x.as_array()) {
                    for e in arr {
                        if let Some(n) = e.as_str() {
                            if !out.iter().any(|x| x == n) {
                                out.push(n.to_string());
                            }
                        }
                    }
                }
            }
            out.sort_by(|a, b| b.parse::<u64>().unwrap_or(0).cmp(&a.parse::<u64>().unwrap_or(0)));
        }
        "velocity" => {
            let v = get_json(state, "https://fill.papermc.io/v3/projects/velocity").await?;
            if let Some(groups) = v.get("versions").and_then(|x| x.as_object()) {
                for (_g, arr) in groups {
                    if let Some(list) = arr.as_array() {
                        for e in list {
                            if let Some(ver) = e.as_str() {
                                out.push(ver.to_string());
                            }
                        }
                    }
                }
            }
            out.sort_by(|a, b| version_key(b.as_str()).cmp(&version_key(a.as_str())));
            out.dedup();
        }
        "bungeecord" => {
            let v = get_json(
                state,
                "https://hub.spigotmc.org/jenkins/job/BungeeCord/lastSuccessfulBuild/api/json",
            )
            .await?;
            if let Some(n) = v.get("number").and_then(|x| x.as_i64()) {
                out.push(n.to_string());
            }
        }
        "forge" => {
            // 完整构建列表来自 maven-metadata（缓存 10 分钟）
            let builds = forge_builds(state).await?;
            let prefix = format!("{game}-");
            out = builds
                .iter()
                .filter(|b| b.starts_with(&prefix))
                .map(|b| b[prefix.len()..].to_string())
                .collect();
            if out.is_empty() {
                return Err(format!("未找到 Forge {game} 的构建版本"));
            }
            out.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
        }
        "neoforge" => {
            let v = get_json(
                state,
                "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge",
            )
            .await?;
            if let Some(arr) = v.get("versions").and_then(|x| x.as_array()) {
                for e in arr {
                    if let Some(ver) = e.as_str() {
                        if neoforge_mc_version(ver).as_deref() == Some(game) {
                            out.push(ver.to_string());
                        }
                    }
                }
            }
            // 早期 NeoForge（1.20.1）发布在 net/neoforged/forge 坐标下
            if out.is_empty() {
                if let Ok(v) = get_json(
                    state,
                    "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/forge",
                )
                .await
                {
                    if let Some(arr) = v.get("versions").and_then(|x| x.as_array()) {
                        for e in arr {
                            if let Some(ver) = e.as_str() {
                                if neoforge_mc_version(ver).as_deref() == Some(game) {
                                    out.push(ver.to_string());
                                }
                            }
                        }
                    }
                }
            }
            out.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
        }
        _ => return Err(format!("未知加载器: {loader}")),
    }
    Ok(out)
}

async fn forge_builds(state: &AppState) -> Result<Vec<String>, String> {
    {
        let cache = state.forge_builds.lock().unwrap();
        if let Some((at, v)) = cache.as_ref() {
            if at.elapsed() < Duration::from_secs(600) {
                return Ok(v.clone());
            }
        }
    }
    let xml = http(state)
        .get("https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let re = regex::Regex::new(r"<version>([^<]+)</version>").unwrap();
    let list: Vec<String> = re.captures_iter(&xml).map(|c| c[1].to_string()).collect();
    if list.is_empty() {
        return Err("Forge 构建列表为空".into());
    }
    *state.forge_builds.lock().unwrap() = Some((std::time::Instant::now(), list.clone()));
    Ok(list)
}

/// 后台任务：安装模组加载器服务端
pub async fn install(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    loader: &str,
    game: &str,
    loader_ver: &str,
) {
    let result = install_inner(state, job_id, instance_id, loader, game, loader_ver).await;
    match result {
        Ok(()) => crate::jobs::finish_job(state, job_id, None, Some(instance_id.to_string())),
        Err(e) => crate::jobs::finish_job(state, job_id, Some(e), Some(instance_id.to_string())),
    }
}

async fn install_inner(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    loader: &str,
    game: &str,
    loader_ver: &str,
) -> Result<(), String> {
    let rt = crate::instance::get_instance(state, instance_id)
        .await
        .map_err(|e| e.to_string())?;
    let name = match loader {
        "fabric" => "Fabric",
        "quilt" => "Quilt",
        "forge" => "Forge",
        "neoforge" => "NeoForge",
        "paper" => "Paper",
        "purpur" => "Purpur",
        "folia" => "Folia",
        "velocity" => "Velocity",
        "waterfall" => "Waterfall",
        "bungeecord" => "BungeeCord",
        other => return Err(format!("未知加载器: {other}")),
    };
    log_job(
        state,
        job_id,
        format!("开始安装 {name} 服务端（MC {game}，加载器 {loader_ver}）…"),
    );

    match loader {
        "fabric" | "quilt" => {
            let (meta_base, jar_name) = if loader == "fabric" {
                ("https://meta.fabricmc.net/v2", "fabric-server-launch.jar")
            } else {
                ("https://meta.quiltmc.org/v3", "quilt-server-launch.jar")
            };
            let installer = get_json(state, &format!("{meta_base}/versions/installer"))
                .await?
                .as_array()
                .and_then(|a| a.first())
                .and_then(|e| e.get("version").and_then(|x| x.as_str()))
                .ok_or("未获取到安装器版本")?
                .to_string();
            let url = format!("{meta_base}/versions/loader/{game}/{loader_ver}/{installer}/server/jar");
            log_job(state, job_id, "下载官方服务端启动器 jar…");
            let dest = rt.dir.join(jar_name);
            download_simple(state, &url, &dest).await?;
            {
                let mut meta = rt.meta.write().await;
                meta.jar = Some(jar_name.to_string());
            }
            rt.persist().await.map_err(|e| e.to_string())?;
            log_job(
                state,
                job_id,
                format!("✅ {name} 服务端安装完成！已配置主程序 {jar_name}。首次启动会自动下载 Minecraft 与依赖库（需要几分钟），请耐心等待。"),
            );
        }
        "paper" | "folia" | "waterfall" | "velocity" => {
            // loader_ver：Paper 系为构建号；Velocity 为自身版本号
            let (url, jar_name) = if loader == "velocity" {
                let vdetail = get_json(
                    state,
                    &format!("https://fill.papermc.io/v3/projects/velocity/versions/{loader_ver}/builds"),
                )
                .await?;
                let builds = vdetail.as_array().ok_or("未获取到 Velocity 构建列表")?;
                let dl = builds
                    .iter()
                    .find_map(|b| {
                        b.get("downloads")
                            .and_then(|d| d.get("server:default").or_else(|| {
                                d.as_object().and_then(|o| o.values().next())
                            }))
                            .and_then(|d| d.get("url").and_then(|x| x.as_str()))
                            .map(|s| s.to_string())
                    })
                    .ok_or("未获取到 Velocity 下载地址")?;
                (dl, "velocity.jar".to_string())
            } else {
                let builds = get_json(
                    state,
                    &format!("https://fill.papermc.io/v3/projects/{loader}/versions/{game}/builds"),
                )
                .await?;
                let arr = builds.as_array().ok_or("未获取到构建列表")?;
                let dl = arr
                    .iter()
                    .find(|b| {
                        b.get("id").and_then(|x| x.as_i64()).map(|x| x.to_string()) == Some(loader_ver.to_string())
                    })
                    .and_then(|b| {
                        b.get("downloads")
                            .and_then(|d| d.get("server:default").or_else(|| {
                                d.as_object().and_then(|o| o.values().next())
                            }))
                            .and_then(|d| d.get("url").and_then(|x| x.as_str()))
                            .map(|s| s.to_string())
                    })
                    .ok_or(format!("未找到构建 {loader_ver} 的下载地址"))?;
                (dl, format!("{loader}.jar"))
            };
            log_job(state, job_id, "下载官方服务端 jar…");
            let dest = rt.dir.join(&jar_name);
            download_simple(state, &url, &dest).await?;
            {
                let mut meta = rt.meta.write().await;
                meta.jar = Some(jar_name.clone());
            }
            rt.persist().await.map_err(|e| e.to_string())?;
            log_job(
                state,
                job_id,
                format!("✅ {name} 服务端安装完成！已配置主程序 {jar_name}。Paper 系首次启动会自动下载 Vanilla 服务端与依赖（需要几分钟）。"),
            );
        }
        "purpur" => {
            let url = format!("https://api.purpurmc.org/v2/purpur/{game}/{loader_ver}/download");
            log_job(state, job_id, "下载官方服务端 jar…");
            let dest = rt.dir.join("purpur.jar");
            download_simple(state, &url, &dest).await?;
            {
                let mut meta = rt.meta.write().await;
                meta.jar = Some("purpur.jar".into());
            }
            rt.persist().await.map_err(|e| e.to_string())?;
            log_job(state, job_id, "✅ Purpur 服务端安装完成！已配置主程序 purpur.jar。");
        }
        "bungeecord" => {
            let url = "https://hub.spigotmc.org/jenkins/job/BungeeCord/lastSuccessfulBuild/artifact/bootstrap/target/BungeeCord.jar";
            log_job(state, job_id, "下载 BungeeCord…");
            let dest = rt.dir.join("BungeeCord.jar");
            download_simple(state, &url, &dest).await?;
            {
                let mut meta = rt.meta.write().await;
                meta.jar = Some("BungeeCord.jar".into());
            }
            rt.persist().await.map_err(|e| e.to_string())?;
            log_job(state, job_id, "✅ BungeeCord 安装完成！已配置主程序 BungeeCord.jar。");
        }
        "forge" | "neoforge" => {
            let installer_urls: Vec<String> = if loader == "forge" {
                let full = format!("{game}-{loader_ver}");
                vec![
                    format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{full}/forge-{full}-installer.jar"),
                    format!("https://bmclapi2.bangbang93.com/maven/net/minecraftforge/forge/{full}/forge-{full}-installer.jar"),
                ]
            } else {
                vec![format!(
                    "https://maven.neoforged.net/releases/net/neoforged/neoforge/{loader_ver}/neoforge-{loader_ver}-installer.jar"
                )]
            };
            let inst_path = rt.dir.join(format!("{loader}-installer.jar.part"));
            log_job(state, job_id, "下载官方安装器…");
            download_first(state, &installer_urls, &inst_path).await?;

            let meta = rt.meta.read().await.clone();
            let java = meta
                .java_path
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "java".into());
            log_job(
                state,
                job_id,
                format!("运行官方安装器（--installServer，首次安装需联网下载库文件，可能需要几分钟）…"),
            );
            run_installer(state, job_id, &java, &inst_path, &rt.dir).await?;
            let _ = tokio::fs::remove_file(&inst_path).await;

            log_job(state, job_id, "检测安装结果并配置启动方式…");
            let jars = files::list_jars(&rt.dir);
            let (jar, jvm_args) = crate::instance::modpack::detect_launch(&rt.dir, &jars);
            {
                let mut m = rt.meta.write().await;
                m.jar = jar.clone();
                m.jvm_args = jvm_args.clone().unwrap_or_default();
            }
            rt.persist().await.map_err(|e| e.to_string())?;
            match (&jar, &jvm_args) {
                (Some(j), _) => log_job(state, job_id, format!("✅ {name} 安装完成！主程序: {j}")),
                (None, Some(a)) => log_job(state, job_id, format!("✅ {name} 安装完成！启动参数: {a}")),
                _ => log_job(state, job_id, format!("⚠ 安装完成但未识别到启动方式，请在实例设置中手动配置")),
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

async fn run_installer(
    state: &AppState,
    job_id: &str,
    java: &str,
    installer: &Path,
    dir: &Path,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut cmd = tokio::process::Command::new(java);
    cmd.arg("-jar")
        .arg(installer)
        .arg("--installServer")
        .arg(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::util::no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法运行安装器: {e}（请检查实例设置中的 Java 路径）"))?;
    if let Some(stdout) = child.stdout.take() {
        let st = state.clone();
        let jid = job_id.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if !line.trim().is_empty() {
                    log_job(&st, &jid, format!("[安装器] {line}"));
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let st = state.clone();
        let jid = job_id.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if !line.trim().is_empty() {
                    log_job(&st, &jid, format!("[安装器/stderr] {line}"));
                }
            }
        });
    }
    let status = child
        .wait()
        .await
        .map_err(|e| format!("等待安装器退出失败: {e}"))?;
    if !status.success() {
        return Err(format!(
            "安装器退出码异常: {}（请检查 Java 版本是否满足该 MC 版本要求，详见日志）",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "未知".into())
        ));
    }
    Ok(())
}

async fn download_simple(state: &AppState, url: &str, dest: &Path) -> Result<(), String> {
    let resp = http(state)
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("下载失败: {e}"))?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("下载中断: {e}"))?;
    tokio::fs::write(dest, &bytes)
        .await
        .map_err(|e| format!("写入文件失败: {e}"))?;
    Ok(())
}

async fn download_first(state: &AppState, urls: &[String], dest: &Path) -> Result<(), String> {
    let mut last = String::new();
    for u in urls {
        match download_simple(state, u, dest).await {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(format!("下载安装器失败（官方源与镜像均不可达）: {last}"))
}
