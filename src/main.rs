mod alerts;
mod api;
mod audit;
mod config;
mod error;
mod instance;
mod java_scan;
mod jobs;
mod rcon;
mod state;
mod util;

use anyhow::Context;

fn humansize(bytes: u64) -> String {
    if bytes >= 1073741824 { format!("{:.2} GB", bytes as f64 / 1073741824.0) } else { format!("{:.0} MB", bytes as f64 / 1048576.0) }
}
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::load_or_create()?;
    let app_state = state::AppState::new(cfg.clone()).await?;

    // 清理上次运行遗留的整合包预览临时目录（超过 1 小时即过期）
    {
        let tmp = std::env::temp_dir();
        if let Ok(rd) = std::fs::read_dir(&tmp) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("mcspr_pack_") && !name.starts_with("mcspr_packupload_") {
                    continue;
                }
                let expired = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(true))
                    .unwrap_or(false);
                if expired {
                    let p = e.path();
                    let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                    tracing::info!("已清理过期临时文件: {}", name);
                }
            }
        }
    }

    let addr = cfg.listen.clone();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("无法监听 {addr}（端口可能被占用）"))?;
    tracing::info!("MCS Panel 启动成功: http://{}", listener.local_addr()?);
    tracing::info!("实例数据目录: {}", cfg.instances_dir().display());

    let app = api::router(app_state.clone());

    // 自动拉起开启了「面板启动时自动运行」的实例（间隔 5 秒逐个启动，避免端口冲突）
    {
        let instances = app_state.instances.read().await.clone();
        let mut delay: u64 = 0;
        for (id, rt) in instances {
            if !rt.meta.read().await.auto_start_on_boot {
                continue;
            }
            let name = rt.meta.read().await.name.clone();
            if delay == 0 {
                tracing::info!("实例「{name}」将在面板就绪后自动启动");
            } else {
                tracing::info!("实例「{name}」将在面板就绪 {delay} 秒后自动启动");
            }
            let st = app_state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3 + delay)).await;
                if let Err(e) = crate::instance::process::start(st, rt).await {
                    tracing::warn!("实例 {id} 自动启动失败: {e}");
                }
            });
            delay += 5;
        }
    }

    // 面板退出时优雅停止所有运行中的实例（世界落盘），避免孤儿进程与文件锁
    // 可观测性：TPS 采样（10s，走 RCON）与 CPU/内存历史采样（30s）
    {
        let st = app_state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                // 磁盘告警检查
                {
                    let dd = {
                        let c = st.config.read().await;
                        c.data_dir.clone()
                    };
                    let warn = { st.config.read().await.thresholds.disk_warn_percent };
                    if let (Ok(total), Ok(free)) = (fs4::total_space(&dd), fs4::available_space(&dd)) {
                        if total > 0 {
                            let used_pct = ((total - free) * 100 / total) as u32;
                            if used_pct >= warn {
                                alerts::send(&st, "disk", format!("磁盘已用 {used_pct}%（告警线 {warn}%），可用空间 {}", humansize(free))).await;
                            }
                        }
                    }
                }
                let map = st.instances.read().await.clone();
                for (_id, rt) in map {
                    if *rt.status.lock().await == instance::Status::Stopped {
                        continue;
                    }
                    let dir = rt.dir.clone();
                    let runtime = rt.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        if rcon::rcon_config(&dir).is_none() {
                            return serde_json::json!({ "needs_rcon": true });
                        }
                        match rcon::sample_tps(&runtime.rcon, &dir) {
                            Ok((tps, mspt)) => serde_json::json!({ "tps": tps, "mspt": mspt }),
                            Err(error) => serde_json::json!({ "error": error }),
                        }
                    })
                    .await
                    .ok();
                    if let Some(v) = res {
                        *rt.tps.lock().await = Some(v);
                    }
                }
            }
        });
    }
    {
        let st = app_state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let map = st.instances.read().await.clone();
                let mut running: Vec<(String, std::sync::Arc<instance::InstanceRuntime>)> =
                    Vec::new();
                for (id, rt) in map {
                    if *rt.status.lock().await != instance::Status::Stopped {
                        running.push((id, rt));
                    }
                }
                if running.is_empty() {
                    continue;
                }
                let pids: Vec<sysinfo::Pid> = running
                    .iter()
                    .filter_map(|(_, rt)| {
                        let p = rt.pid.load(std::sync::atomic::Ordering::SeqCst);
                        (p != 0).then(|| sysinfo::Pid::from_u32(p))
                    })
                    .collect();
                if pids.is_empty() {
                    continue;
                }
                {
                    // metrics 写入（rt.metrics 锁）在 sys 锁的作用域外逐个进行
                    let samples: Vec<(u32, f32, f64)> = {
                        let mut sys = st.sys.lock().unwrap_or_else(|p| p.into_inner());
                        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
                        running
                            .iter()
                            .map(|(_, rt)| {
                                let pid = rt.pid.load(std::sync::atomic::Ordering::SeqCst);
                                let (cpu, mem) = match sys.process(sysinfo::Pid::from_u32(pid)) {
                                    Some(p) => (p.cpu_usage(), p.memory() as f64 / 1048576.0),
                                    None => (0.0, 0.0),
                                };
                                (pid, cpu, mem)
                            })
                            .collect()
                    };
                    for ((_, rt), (pid, cpu, mem)) in running.iter().zip(samples.iter()) {
                        let _ = pid;
                        let ts = chrono::Utc::now().timestamp();
                        let mut m = rt.metrics.lock().await;
                        m.push_back((ts, *cpu, *mem));
                        while m.len() > 2880 {
                            m.pop_front();
                        }
                    }
                }
            }
        });
    }

    // 计划任务调度器（每 30 秒检查一次到期任务）
    {
        let st = app_state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let map = st.instances.read().await.clone();
                for (_id, rt) in map {
                    if *rt.status.lock().await == instance::Status::Stopped
                        && !std::path::Path::new(&rt.dir).join("tasks.json").exists()
                    {
                        continue;
                    }
                    let st2 = st.clone();
                    tokio::spawn(async move {
                        instance::tasks::run_due(&st2, &rt).await;
                    });
                }
            }
        });
    }

    // 可观测性：TPS 采样（10s，走 RCON）与 CPU/内存历史采样（30s）占位结束
    tracing::info!("按 Ctrl+C 停止面板时会自动保存并停止所有运行中的服务器");
    let shutdown = instance::process::shutdown_all(&app_state);
    tokio::select! {
        r = axum::serve(listener, app) => {
            r.context("HTTP 服务异常退出")?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到退出信号，正在停止所有运行中的实例…");
            shutdown.await;
            tracing::info!("全部实例已停止，面板退出");
        }
    }
    Ok(())
}
