//! 实例公告：Markdown 安全渲染与编辑接口

use crate::error::{ApiError, ApiResult};
use crate::instance::{get_instance, InstanceMeta, InstanceRuntime};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// 公告内容上限（64 KiB）
pub const MAX_ANNOUNCEMENT_BYTES: usize = 64 * 1024;

/// 将 Markdown 渲染为经过清理的安全 HTML。
/// 原始 HTML（含 `<script>` 与事件属性）被整体丢弃，只保留 Markdown 生成的结构。
pub fn render_markdown(markdown: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(markdown, opts)
        .filter(|e| !matches!(e, Event::Html(_) | Event::InlineHtml(_)));
    let mut out = String::new();
    html::push_html(&mut out, parser);
    sanitize_html(&out)
}

fn sanitize_html(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(["table", "thead", "tbody", "tfoot", "tr", "th", "td"])
        .url_schemes(std::collections::HashSet::from(["http", "https", "mailto"]))
        .url_relative(ammonia::UrlRelative::Deny)
        .attribute_filter(|element, attribute, value| {
            // 图片只允许 http/https 来源，拒绝 data: / ftp: 等内联或第三方协议
            if element == "img" && attribute == "src" {
                let lower = value.trim().to_ascii_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    return Some(value.into());
                }
                return None;
            }
            Some(value.into())
        })
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(html)
        .to_string()
}

fn announcement_json(meta: &InstanceMeta) -> Value {
    json!({
        "markdown": meta.announcement_markdown,
        "html": render_markdown(&meta.announcement_markdown),
        "updated_at": meta.announcement_updated_at,
        "updated_by": meta.announcement_updated_by,
    })
}

/// 读取实例公告（授权用户）
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let rt = get_instance(&state, &id).await?;
    let meta = rt.meta.read().await;
    Ok(Json(announcement_json(&meta)))
}

#[derive(Deserialize)]
pub struct PutReq {
    pub markdown: String,
    pub expected_updated_at: String,
}

/// 保存公告（管理员）。`expected_updated_at` 与当前值不符时返回 409。
pub async fn put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<crate::auth::Identity>,
    Json(req): Json<PutReq>,
) -> Result<Json<Value>, Response> {
    let rt = get_instance(&state, &id)
        .await
        .map_err(IntoResponse::into_response)?;
    save_announcement(&rt, &req.markdown, &req.expected_updated_at, &identity.username)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

#[derive(Deserialize)]
pub struct PreviewReq {
    pub markdown: String,
}

/// 预览公告渲染结果（管理员）
pub async fn preview(Json(req): Json<PreviewReq>) -> Result<Json<Value>, Response> {
    if req.markdown.len() > MAX_ANNOUNCEMENT_BYTES {
        return Err(ApiError::bad_request(format!(
            "公告内容过长（上限 {} KiB）",
            MAX_ANNOUNCEMENT_BYTES / 1024
        ))
        .into_response());
    }
    Ok(Json(json!({ "html": render_markdown(&req.markdown) })))
}

#[derive(Debug)]
enum SaveError {
    Conflict(String),
    Api(ApiError),
}

impl IntoResponse for SaveError {
    fn into_response(self) -> Response {
        match self {
            SaveError::Conflict(msg) => {
                (StatusCode::CONFLICT, Json(json!({ "error": msg }))).into_response()
            }
            SaveError::Api(e) => e.into_response(),
        }
    }
}

/// 生成严格单调的公告时间戳：同一秒内的连续保存也必须得到不同值，
/// 否则旧的 expected_updated_at 会被误判为最新而静默接受过期编辑。
fn next_announcement_timestamp(previous: &str) -> String {
    let mut now = chrono::Utc::now();
    if let Ok(prev) = chrono::DateTime::parse_from_rfc3339(previous) {
        let prev = prev.with_timezone(&chrono::Utc);
        if now <= prev {
            now = prev + chrono::Duration::nanoseconds(1);
        }
    }
    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

async fn save_announcement(
    rt: &Arc<InstanceRuntime>,
    markdown: &str,
    expected_updated_at: &str,
    author: &str,
) -> Result<Value, SaveError> {
    if markdown.len() > MAX_ANNOUNCEMENT_BYTES {
        return Err(SaveError::Api(ApiError::bad_request(format!(
            "公告内容过长（上限 {} KiB）",
            MAX_ANNOUNCEMENT_BYTES / 1024
        ))));
    }
    // 写锁贯穿校验、落盘与内存提交：并发保存被串行化，
    // 落盘失败时内存保持原值，不会出现「已改内存但未持久化」的中间态。
    let mut meta = rt.meta.write().await;
    if meta.announcement_updated_at != expected_updated_at {
        return Err(SaveError::Conflict(
            "公告已被其他管理员修改，请刷新后重试".to_string(),
        ));
    }
    let mut next = meta.clone();
    next.announcement_markdown = markdown.to_string();
    next.announcement_updated_at = next_announcement_timestamp(&meta.announcement_updated_at);
    next.announcement_updated_by = author.to_string();
    rt.persist_snapshot(&next).await.map_err(SaveError::Api)?;
    *meta = next.clone();
    Ok(announcement_json(&next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_events_and_unsafe_urls() {
        let html = render_markdown(
            "hello\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\n[click](javascript:alert(1))\n\nbefore <b>bold</b> after",
        );
        assert!(html.contains("hello"));
        assert!(html.contains("bold"));
        assert!(!html.contains("<script"));
        assert!(!html.to_lowercase().contains("onerror"));
        assert!(!html.to_lowercase().contains("javascript:"));
        assert!(!html.contains("<b>"));
    }

    #[test]
    fn renders_tables_and_links_safely() {
        let html = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n\n[ok](https://example.com)");
        assert!(html.contains("<table>"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("rel=\"noopener noreferrer nofollow\""));
    }

    #[tokio::test]
    async fn empty_expected_allows_first_save_then_conflict() {
        let dir = std::env::temp_dir().join(format!("mcspr-ann-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = InstanceRuntime::new(Default::default(), dir.clone());

        let saved = save_announcement(&rt, "# hi", "", "admin").await.unwrap();
        assert_eq!(saved["markdown"], "# hi");
        let first_ts = saved["updated_at"].as_str().unwrap().to_string();
        assert!(!first_ts.is_empty());

        // 首次保存后，空的 expected 必须冲突
        let stale = save_announcement(&rt, "changed", "", "admin").await;
        assert!(matches!(stale, Err(SaveError::Conflict(_))));

        // 同一秒内的第二次保存也必须产生不同时间戳
        let second = save_announcement(&rt, "changed", &first_ts, "admin").await.unwrap();
        let second_ts = second["updated_at"].as_str().unwrap().to_string();
        assert_ne!(first_ts, second_ts, "同一秒内的两次保存也必须得到不同时间戳");

        // 用已过期的第一个时间戳再次保存必须 409（即使仍在同一秒）
        let third = save_announcement(&rt, "third", &first_ts, "admin").await;
        assert!(matches!(third, Err(SaveError::Conflict(_))));

        // 最新时间戳可以继续保存
        let fourth = save_announcement(&rt, "fourth", &second_ts, "admin").await.unwrap();
        assert_eq!(fourth["markdown"], "fourth");
        let fourth_ts = fourth["updated_at"].as_str().unwrap().to_string();
        assert_ne!(second_ts, fourth_ts);

        // second_ts 现已过期：再次使用必须 409
        let fifth = save_announcement(&rt, "fifth", &second_ts, "admin").await;
        assert!(matches!(fifth, Err(SaveError::Conflict(_))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn oversized_announcement_is_rejected() {
        let dir = std::env::temp_dir().join(format!("mcspr-ann-big-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = InstanceRuntime::new(Default::default(), dir.clone());
        let big = "x".repeat(MAX_ANNOUNCEMENT_BYTES + 1);
        let err = save_announcement(&rt, &big, "", "admin").await;
        assert!(matches!(err, Err(SaveError::Api(ApiError::Bad(_)))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restricts_url_schemes_and_image_sources() {
        let html = render_markdown(
            "[a](ftp://evil/x)\n\n![i](ftp://evil/x.png)\n\n[b](data:text/html;base64,AAA)\n\n[m](mailto:me@example.com)\n\n![ok](https://cdn.example.com/x.png)",
        );
        assert!(!html.contains("ftp:"), "ftp 链接必须被移除: {html}");
        assert!(!html.contains("data:"), "data: 链接必须被移除: {html}");
        assert!(html.contains("href=\"mailto:me@example.com\""));
        assert!(html.contains("src=\"https://cdn.example.com/x.png\""));
    }

    #[tokio::test]
    async fn failed_persistence_keeps_previous_announcement() {
        let dir = std::env::temp_dir().join(format!("mcspr-ann-fail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = InstanceRuntime::new(Default::default(), dir.clone());
        let saved = save_announcement(&rt, "first", "", "admin").await.unwrap();
        let ts = saved["updated_at"].as_str().unwrap().to_string();

        // 破坏实例目录：在目录位置放一个文件，使 instance.json 无法写入
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();
        let err = save_announcement(&rt, "second", &ts, "admin").await;
        assert!(matches!(err, Err(SaveError::Api(_))));

        let meta = rt.meta.read().await;
        assert_eq!(meta.announcement_markdown, "first");
        assert_eq!(meta.announcement_updated_at, ts);
        drop(meta);
        let _ = std::fs::remove_file(&dir);
    }

    #[tokio::test]
    async fn concurrent_same_expected_allows_only_one_save() {
        let dir = std::env::temp_dir().join(format!("mcspr-ann-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = InstanceRuntime::new(Default::default(), dir.clone());

        let rt_a = rt.clone();
        let rt_b = rt.clone();
        let a = tokio::spawn(async move { save_announcement(&rt_a, "A", "", "admin").await });
        let b = tokio::spawn(async move { save_announcement(&rt_b, "B", "", "admin").await });
        let (ra, rb) = tokio::join!(a, b);
        let a_conflict = matches!(ra.unwrap(), Err(SaveError::Conflict(_)));
        let b_conflict = matches!(rb.unwrap(), Err(SaveError::Conflict(_)));
        assert_ne!(a_conflict, b_conflict, "相同 expected 必须恰好一个成功、一个冲突");

        let meta = rt.meta.read().await;
        let winner = meta.announcement_markdown.clone();
        assert!(winner == "A" || winner == "B");
        drop(meta);
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("instance.json")).unwrap()).unwrap();
        assert_eq!(persisted["announcement_markdown"], winner);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
