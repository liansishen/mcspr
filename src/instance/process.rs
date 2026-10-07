use super::{InstanceRuntime, LogLine, Status};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;

async fn spawn_server(rt: &Arc<InstanceRuntime>) -> ApiResult<tokio::process::Child> {
    let meta = rt.meta.read().await.clone();
    let jvm_args = meta.jvm_args.trim().to_string();
    let jar = meta.jar.clone().unwrap_or_default().trim().to_string();

    if jar.is_empty() && jvm_args.is_empty() {
        return Err(ApiError::bad_request(
            "未配置启动方式：请在「实例设置」中选择主程序 JAR 或填写启动参数",
        ));
    }
    if !jar.is_empty() && !rt.dir.join(&jar).exists() {
        return Err(ApiError::bad_request(format!("主程序不存在: {jar}")));
    }

    let java = meta
        .java_path
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "java".to_string());
    let mut cmd = Command::new(&java);
    cmd.arg(format!("-Xms{}M", meta.min_ram_mb))
        .arg(format!("-Xmx{}M", meta.max_ram_mb));
    for a in jvm_args.split_whitespace() {
        cmd.arg(a);
    }
    if !jar.is_empty() {
        cmd.arg("-jar").arg(&jar);
    }
    // nogui 防止弹出 GUI 窗口（Velocity/BungeeCord 等代理端会忽略此参数）
    cmd.arg("nogui");
    cmd.current_dir(&rt.dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW，避免弹出控制台窗口

    cmd.spawn()
        .map_err(|e| ApiError::internal(format!("无法启动 Java 进程: {e}（请检查实例设置中的 Java 路径）")))
}

/// start 不再是 async fn（返回 boxed dyn Future），切断 start ↔ on_exit 的
/// 异步递归类型循环（自动重启会再次调用 start）
pub fn start(
    state: AppState,
    rt: Arc<InstanceRuntime>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApiResult<()>> + Send>> {
    Box::pin(start_inner(state, rt))
}

/// 解码进程输出：正常按 UTF-8；失败（中文 Windows 上 Java 启动器输出 GBK）则回退 GBK
fn decode_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::GBK.decode(bytes);
            cow.into_owned()
        }
    }
}

async fn start_inner(state: AppState, rt: Arc<InstanceRuntime>) -> ApiResult<()> {
    let id = rt.meta.read().await.id.clone();
    let _busy = state.busy_guard(&id).ok_or_else(|| {
        ApiError::bad_request("该实例有整体操作正在进行，请稍候再启动")
    })?;
    {
        let mut st = rt.status.lock().await;
        if *st != Status::Stopped {
            return Err(ApiError::bad_request("实例已在运行中"));
        }
        *st = Status::Starting;
    }

    let mut child = match spawn_server(&rt).await {
        Ok(c) => c,
        Err(e) => {
            *rt.status.lock().await = Status::Stopped;
            return Err(e);
        }
    };

    let pid = child.id().unwrap_or(0);
    *rt.stdin.lock().await = child.stdin.take();
    rt.stopping.store(false, Ordering::SeqCst);
    rt.ready.store(false, Ordering::SeqCst);
    *rt.started_at.lock().await = Some(chrono::Utc::now());
    rt.pid.store(pid, Ordering::SeqCst);

    // 自动重启：全新启动时清空崩溃计数与风暴标记
    rt.crash_times.lock().await.clear();
    rt.restart_storm.store(false, Ordering::SeqCst);

    if let Some(stdout) = child.stdout.take() {
        let rt1 = rt.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = decode_bytes(&buf).trim_end_matches(['\r', '\n']).to_string();
                if !line.is_empty() {
                    push_log(&rt1, line).await;
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let rt2 = rt.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = decode_bytes(&buf).trim_end_matches(['\r', '\n']).to_string();
                if !line.is_empty() {
                    push_log(&rt2, format!("[stderr] {line}")).await;
                }
            }
        });
    }

    let rt3 = rt.clone();
    let st3 = state.clone();
    tokio::spawn(async move {
        let code = child.wait().await;
        on_exit(st3, rt3, code).await;
    });

    push_log(
        &rt,
        format!("[面板] 进程已启动 (PID {pid})，内存限制: {}M ~ {}M", rt.meta.read().await.min_ram_mb, rt.meta.read().await.max_ram_mb),
    )
    .await;
    Ok(())
}

async fn on_exit(
    state: AppState,
    rt: Arc<InstanceRuntime>,
    res: std::io::Result<std::process::ExitStatus>,
) {
    let was_ready = rt.ready.load(Ordering::SeqCst);
    let stopping = rt.stopping.load(Ordering::SeqCst);
    let auto = rt.meta.read().await.auto_restart;

    *rt.stdin.lock().await = None;
    // 服务端已退出，缓存的 RCON 连接随之失效
    *rt.rcon.lock().unwrap_or_else(|p| p.into_inner()) = None;
    rt.ready.store(false, Ordering::SeqCst);
    rt.players.lock().await.clear();
    rt.pid.store(0, Ordering::SeqCst);
    *rt.started_at.lock().await = None;
    *rt.status.lock().await = Status::Stopped;

    // 结算未闭合的玩家在线会话（可观测性）
    settle_all_sessions(&rt).await;

    // 崩溃归档（异常退出时保存控制台末尾 + crash-report）
    if !stopping {
        archive_crash(&rt).await;
    }

    let code = res.ok().and_then(|s| s.code());
    let clean_exit = stopping || code == Some(0);
    if clean_exit {
        push_log(&rt, "[面板] 进程已停止".into()).await;
    } else {
        let code_str = code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "未知".into());
        push_log(&rt, format!("[面板] 进程已退出 (code: {code_str})")).await;
        if code.map(|c| c != 0).unwrap_or(true) {
            let _name = rt.meta.read().await.name.clone();
            let name = rt.meta.read().await.name.clone();
            crate::alerts::send(&state, &format!("crash-{}", rt.meta.read().await.id), format!("实例「{name}」异常退出 (code: {code_str})")).await;
        }

        // 重启风暴熔断：窗口内连崩达到阈值则停止自动重启
        let (window, max, delay) = {
            let c = state.config.read().await;
            (
                c.thresholds.crash_window_secs,
                c.thresholds.crash_max,
                c.thresholds.restart_delay_secs,
            )
        };
        let storm = {
            let mut times = rt.crash_times.lock().await;
            let now = std::time::Instant::now();
            times.push_back(now);
            while let Some(t) = times.front() {
                if now.duration_since(*t).as_secs() > window {
                    times.pop_front();
                } else {
                    break;
                }
            }
            times.len() >= max as usize
        };
        if storm {
            rt.restart_storm.store(true, Ordering::SeqCst);
            push_log(
                &rt,
                format!(
                    "[面板] 检测到重启风暴：{window} 秒内已连续崩溃 {max} 次，已停止自动重启。请排查崩溃原因后在实例页手动启动。"
                ),
            )
            .await;
        } else if was_ready && auto {
            push_log(&rt, format!("[面板] {delay} 秒后自动重启…").into()).await;
            let st2 = state.clone();
            let rt2 = rt.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                let cur = *rt2.status.lock().await;
                if cur == Status::Stopped {
                    let _ = start(st2, rt2).await;
                }
            });
        }
    }
}

pub async fn stop(_state: AppState, rt: Arc<InstanceRuntime>) -> ApiResult<()> {
    {
        let st = *rt.status.lock().await;
        if st == Status::Stopped {
            return Err(ApiError::bad_request("实例未在运行"));
        }
        if st == Status::Stopping {
            return Err(ApiError::bad_request("实例正在停止中"));
        }
    }
    *rt.status.lock().await = Status::Stopping;
    rt.stopping.store(true, Ordering::SeqCst);
    push_log(&rt, "[面板] 正在发送 stop 命令…".into()).await;
    let _ = send_command(&rt, "stop").await;

    // 最多等 20 秒优雅退出
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if *rt.status.lock().await == Status::Stopped {
            return Ok(());
        }
    }
    let pid = rt.pid.load(Ordering::SeqCst);
    push_log(&rt, format!("[面板] 未能正常停止，强制结束进程 {pid}")).await;
    force_kill(pid);
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if *rt.status.lock().await == Status::Stopped {
            break;
        }
    }
    Ok(())
}

fn force_kill(pid: u32) {
    if pid == 0 {
        return;
    }
    tracing::warn!("强制终止进程 {pid}");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
    }
}

pub async fn send_command(rt: &Arc<InstanceRuntime>, cmd: &str) -> ApiResult<()> {
    let trimmed = cmd.trim();
    if trimmed.eq_ignore_ascii_case("stop") || trimmed.eq_ignore_ascii_case("/stop") {
        rt.stopping.store(true, Ordering::SeqCst);
        *rt.status.lock().await = Status::Stopping;
    }
    let mut g = rt.stdin.lock().await;
    match g.as_mut() {
        Some(stdin) => {
            stdin
                .write_all(format!("{cmd}\n").as_bytes())
                .await
                .map_err(|e| ApiError::internal(format!("命令发送失败: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| ApiError::internal(format!("命令发送失败: {e}")))?;
            Ok(())
        }
        None => Err(ApiError::bad_request("实例未在运行，无法发送命令")),
    }
}

pub async fn push_log(rt: &Arc<InstanceRuntime>, line: String) {
    let eula_hint = line.contains("you need to agree to the EULA");
    let java_hint = java_version_hint(&line);
    let port_hint = port_bind_hint(&line);
    push_raw(rt, line).await;
    if eula_hint {
        push_raw(rt, "[面板] 检测到需要同意 EULA：请在实例页点击「同意 EULA」后重新启动".into()).await;
    }
    if let Some(hint) = java_hint {
        push_raw(rt, hint).await;
    }
    if let Some(hint) = port_hint {
        push_raw(rt, hint).await;
    }
}

/// 检测端口绑定失败，给出中文提示
fn port_bind_hint(line: &str) -> Option<String> {
    if line.contains("FAILED TO BIND TO PORT") || line.contains("BindException") {
        return Some(
            "[面板] 端口已被占用！可能原因：① 另一个服务器实例正在使用同一端口；② 之前的服务器进程未完全退出。请修改 server.properties 中的 server-port，或关闭占用端口的进程。".to_string(),
        );
    }
    None
}

/// 检测 Java 版本不匹配（如 UnsupportedClassVersionError），给出中文提示
fn java_version_hint(line: &str) -> Option<String> {
    if !line.contains("UnsupportedClassVersionError") {
        return None;
    }
    // class file version 69.0 → 69 - 44 = Java 25
    let re = regex::Regex::new(r"class file version (\d+)").ok()?;
    let required = re
        .captures(line)?
        .get(1)?
        .as_str()
        .parse::<u32>()
        .ok()?
        .saturating_sub(44);
    Some(format!(
        "[面板] Java 版本不匹配：该服务端需要 Java {required}，当前 Java 版本过低。请在「实例设置 → Java 路径」中更换（可从扫描结果中选择更高版本）。"
    ))
}

async fn push_raw(rt: &Arc<InstanceRuntime>, line: String) {
    if line.contains("Stopping the server") || line.contains("Stopping server") {
        rt.stopping.store(true, Ordering::SeqCst);
    }
    let seq = rt.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let ll = LogLine {
        seq,
        ts: chrono::Local::now().format("%H:%M:%S").to_string(),
        line: line.clone(),
    };
    {
        let limit = rt.log_limit.load(Ordering::Relaxed).max(1);
        let mut buf = rt.log_buf.lock().await;
        buf.push_back(ll.clone());
        while buf.len() > limit {
            buf.pop_front();
        }
    }
    // 面板控制台日志落盘（超 16MB 轮转），面板自身的 [面板] 行也在其中
    write_panel_log(rt, &ll).await;
    let _ = rt.log_tx.send(ll);

    if let Some(p) = parse_join(&line) {
        let mut players = rt.players.lock().await;
        if !players.contains(&p) {
            players.push(p.clone());
        }
        drop(players);
        // 开始在线时长会话
        rt.open_sessions
            .lock()
            .await
            .insert(p, chrono::Utc::now());
    }
    if let Some(p) = parse_leave(&line) {
        rt.players.lock().await.retain(|x| *x != p);
        // 结算该玩家的本次在线时长
        if let Some(start) = rt.open_sessions.lock().await.remove(&p) {
            settle_playtime(rt, &p, start).await;
        }
    }
    if line.contains("Done (") {
        rt.ready.store(true, Ordering::SeqCst);
        if rt.ready.load(Ordering::SeqCst) {
            *rt.status.lock().await = Status::Running;
        }
    }
}

/// 累计在线时长并落盘 player-stats.json
async fn settle_playtime(rt: &Arc<InstanceRuntime>, name: &str, start: chrono::DateTime<chrono::Utc>) {
    let secs = (chrono::Utc::now() - start).num_seconds().max(0) as u64;
    {
        let mut pt = rt.playtime.lock().await;
        let e = pt.entry(name.to_string()).or_insert((0, 0));
        e.0 += secs;
        e.1 += 1;
    }
    persist_playtime(rt).await;
}

/// 退出时结算所有未闭合的会话（按退出时间计）
pub async fn settle_all_sessions(rt: &Arc<InstanceRuntime>) {
    let now = chrono::Utc::now();
    let mut pt = rt.playtime.lock().await;
    let open = rt.open_sessions.lock().await;
    for (name, start) in open.iter() {
        let secs = (now - *start).num_seconds().max(0) as u64;
        let e = pt.entry(name.clone()).or_insert((0, 0));
        e.0 += secs;
        e.1 += 1;
    }
    drop(open);
    drop(pt);
    persist_playtime(rt).await;
}

async fn persist_playtime(rt: &Arc<InstanceRuntime>) {
    let map = rt.playtime.lock().await.clone();
    let path = rt.dir.join("player-stats.json");
    let _ = tokio::fs::write(&path, serde_json::to_string_pretty(&map).unwrap_or_default()).await;
}

/// 崩溃归档：控制台末尾 200 行 + 最新的 crash-report
pub async fn archive_crash(rt: &Arc<InstanceRuntime>) {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dir = rt.dir.join("crash-archive");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let lines: Vec<String> = rt
        .log_buf
        .lock()
        .await
        .iter()
        .rev()
        .take(200)
        .rev()
        .map(|l| format!("{} {}", l.ts, l.line))
        .collect();
    let _ = tokio::fs::write(
        dir.join(format!("crash-{ts}.log")),
        lines.join("\n"),
    )
    .await;
    // 复制最新的 crash-report（启动阶段崩溃不会有这份）
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(rt.dir.join("crash-reports")) {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                if let Ok(modified) = md.modified() {
                    if latest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                        latest = Some((modified, e.path()));
                    }
                }
            }
        }
    }
    if let Some((_, p)) = latest {
        let _ = std::fs::copy(&p, dir.join(format!("crash-report-{ts}.txt")));
    }
}

/// 面板控制台日志落盘（logs/panel-console.log，超 16MB 轮转）
async fn write_panel_log(rt: &Arc<InstanceRuntime>, ll: &LogLine) {
    let dir = rt.dir.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let p = dir.join("panel-console.log");
    if let Ok(md) = std::fs::metadata(&p) {
        if md.len() > 16 * 1024 * 1024 {
            let _ = std::fs::rename(&p, dir.join("panel-console.log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        use std::io::Write;
        let _ = writeln!(f, "{} {}", ll.ts, ll.line);
    }
}

fn parse_join(line: &str) -> Option<String> {
    for marker in [" joined the game", " 加入了游戏"] {
        if let Some(i) = line.find(marker) {
            let name = line[..i]
                .rsplit(|c| c == ' ' || c == ']' || c == ':')
                .find(|s| !s.is_empty())?;
            return Some(name.trim().to_string());
        }
    }
    None
}

fn parse_leave(line: &str) -> Option<String> {
    for marker in [" left the game", " 离开了游戏"] {
        if let Some(i) = line.find(marker) {
            let name = line[..i]
                .rsplit(|c| c == ' ' || c == ']' || c == ':')
                .find(|s| !s.is_empty())?;
            return Some(name.trim().to_string());
        }
    }
    None
}

/// 面板退出时：并行向所有运行中的实例发送 stop，最长等待 15 秒后强杀
pub async fn shutdown_all(state: &AppState) {
    let map = state.instances.read().await.clone();
    let mut tasks: Vec<(String, Arc<InstanceRuntime>)> = Vec::new();
    for (id, rt) in map {
        if *rt.status.lock().await == Status::Stopped {
            continue;
        }
        tracing::info!("停止实例「{}」…", rt.meta.read().await.name);
        rt.stopping.store(true, Ordering::SeqCst);
        *rt.status.lock().await = Status::Stopping;
        let _ = send_command(&rt, "stop").await;
        tasks.push((id, rt));
    }
    if tasks.is_empty() {
        return;
    }
    // 等待全部退出，最多 15 秒
    for _ in 0..60 {
        let mut all_stopped = true;
        for (_, rt) in &tasks {
            if *rt.status.lock().await != Status::Stopped {
                all_stopped = false;
                break;
            }
        }
        if all_stopped {
            tracing::info!("所有实例已停止并保存");
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    for (id, rt) in &tasks {
        if *rt.status.lock().await != Status::Stopped {
            tracing::warn!("实例 {id} 未能优雅退出，强制结束");
            force_kill(rt.pid.load(Ordering::SeqCst));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_buffer_respects_configured_limit() {
        let dir = std::env::temp_dir().join(format!("mcspr-loglimit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = crate::instance::InstanceRuntime::new(Default::default(), dir.clone());
        rt.log_limit.store(3, Ordering::Relaxed);
        for i in 0..6 {
            push_raw(&rt, format!("line {i}")).await;
        }
        let buf = rt.log_buf.lock().await;
        assert_eq!(buf.len(), 3);
        assert_eq!(
            buf.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            vec!["line 3", "line 4", "line 5"]
        );
        drop(buf);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
