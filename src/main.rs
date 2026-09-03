mod api;
mod config;
mod error;
mod instance;
mod java_scan;
mod jobs;
mod state;
mod util;

use anyhow::Context;
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

    let addr = cfg.listen.clone();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("无法监听 {addr}（端口可能被占用）"))?;
    tracing::info!("MCS Panel 启动成功: http://{}", listener.local_addr()?);
    tracing::info!("实例数据目录: {}", cfg.instances_dir().display());

    let app = api::router(app_state);
    axum::serve(listener, app).await?;
    Ok(())
}
