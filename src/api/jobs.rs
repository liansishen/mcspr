//! 任务中心与写操作幂等：任务列表 / 详情 / 重试、操作状态查询与幂等中间件。

use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::jobs::Job;
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::collections::HashMap;

/// 指纹读取的 JSON 请求体上限（超过则拒绝，避免无界缓冲）
const MAX_FINGERPRINT_BODY: usize = 32 * 1024 * 1024;
/// 结果重放缓存上限
const MAX_REPLAY_BODY: usize = 16 * 1024 * 1024;
/// 列表默认 / 最大分页
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

// ---------- 任务列表 / 详情 ----------

pub async fn list(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let offset = q.get("offset").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let admin = identity.is_admin();
    let mut jobs: Vec<Job> = crate::jobs::all(&state)
        .into_iter()
        // 管理员可见全部；普通用户仅可见自己发起的任务
        .filter(|j| admin || j.user_id.as_deref() == Some(identity.user_id.as_str()))
        .collect();
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let total = jobs.len();
    let page: Vec<Value> = jobs
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|j| job_view(&j, admin))
        .collect();
    Json(json!({ "jobs": page, "total": total, "limit": limit, "offset": offset }))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<Value>> {
    let job = find_job(&state, &id)?;
    let admin = identity.is_admin();
    if !admin && job.user_id.as_deref() != Some(identity.user_id.as_str()) {
        // 与不存在任务统一返回 404，避免任务枚举
        return Err(ApiError::not_found("任务不存在"));
    }
    Ok(Json(job_view(&job, admin)))
}

/// 失败任务重试：当前没有可安全自动重放的任务类型，统一标记为需手动重试。
/// 手动重试应复用原操作编号（X-Operation-ID）与原始内容，以保持幂等。
pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<Value>> {
    let job = find_job(&state, &id)?;
    if !identity.is_admin() && job.user_id.as_deref() != Some(identity.user_id.as_str()) {
        return Err(ApiError::not_found("任务不存在"));
    }
    Ok(Json(json!({
        "ok": false,
        "manual": true,
        "kind": job.kind,
        "message": "该任务不支持自动重试；请回到原页面重新发起操作（前端会复用同一操作编号以保持幂等）",
    })))
}

fn find_job(state: &AppState, id: &str) -> ApiResult<Job> {
    crate::jobs::all(state)
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| ApiError::not_found("任务不存在"))
}

/// 统一对外任务视图：普通用户过滤内部路径与文件系统细节。
fn job_view(job: &Job, admin: bool) -> Value {
    let logs = if admin {
        job.logs.clone()
    } else {
        job.logs.iter().map(|l| redact_line(l)).collect()
    };
    let result = if admin {
        job.result.clone()
    } else {
        job.result.clone().map(redact_value)
    };
    let error = if admin {
        job.error.clone()
    } else {
        job.error.clone().map(|e| redact_line(&e))
    };
    json!({
        "id": job.id,
        "status": job.status,
        "progress": job.progress,
        "logs": logs,
        "instance_id": job.instance_id,
        "kind": job.kind,
        "title": job.title,
        "user_id": job.user_id,
        "stage": job.stage,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "finished_at": job.finished_at,
        "result": result,
        "error": error,
    })
}

/// 逐词替换疑似文件系统路径的 token（保留 URL）。
fn redact_line(line: &str) -> String {
    line.split_whitespace()
        .map(|tok| {
            if tok.starts_with("http://") || tok.starts_with("https://") {
                tok.to_string()
            } else if looks_like_path(tok) {
                "[已隐藏路径]".to_string()
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_path(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| {
        c == '"' || c == '\'' || c == '(' || c == ')' || c == '（' || c == '）' || c == ',' || c == '，'
    });
    if t.len() > 3 && t.contains('\\') {
        return true;
    }
    // 绝对路径：以 / 开头且包含更多分隔符
    t.starts_with('/') && t.matches('/').count() >= 2
}

fn redact_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(redact_line(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(redact_value).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, val)| (k, redact_value(val)))
                .collect(),
        ),
        other => other,
    }
}

// ---------- 操作状态查询 ----------

pub async fn operation_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<Value>> {
    let op = crate::operations::lookup(&state, &id, &identity.user_id, identity.is_admin())
        .ok_or_else(|| ApiError::not_found("操作不存在"))?;
    Ok(Json(json!({
        "operation_id": op.id,
        "status": op.status,
        "method": op.method,
        "route": op.route,
        "status_code": op.status_code,
        "job_id": op.job_id,
        "result": op.response,
        "created_at": op.created_at,
        "updated_at": op.updated_at,
        "finished_at": op.finished_at,
    })))
}

// ---------- 幂等中间件 ----------

/// 对所有已认证写请求生效：
/// 1. 在认证 / CSRF / 同源校验（auth_mw）之后运行，先鉴权再重放；
/// 2. 无 `X-Operation-ID` 时保持原有行为；
/// 3. 同编号同内容返回原结果 / 处理中状态，不同内容冲突，不同账户拒绝；
/// 4. 首次请求在副作用前登记，结束后写回响应以便重放。
pub async fn operation_mw(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    if !matches!(method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE) {
        return next.run(req).await;
    }
    // 公开路由（登录等）不参与幂等，避免无意中包裹凭据接口
    if super::classify(&method, req.uri().path()).0 == super::Access::Public {
        return next.run(req).await;
    }
    let Some(op_id) = req
        .headers()
        .get("x-operation-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| crate::operations::is_valid_id(s))
    else {
        return next.run(req).await;
    };
    let Some(identity) = req.extensions().get::<Identity>().cloned() else {
        // auth_mw 未注入身份（理论上不会发生）：放行由后续中间件处理
        return next.run(req).await;
    };

    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let ctype = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_json = ctype
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json");

    let (fingerprint, req) = if is_json {
        let (parts, body) = req.into_parts();
        match axum::body::to_bytes(body, MAX_FINGERPRINT_BODY).await {
            Ok(bytes) => {
                let fp = crate::operations::fingerprint(
                    &state,
                    &identity.user_id,
                    method.as_str(),
                    &path,
                    &query,
                    Some(&bytes),
                );
                (fp, Request::from_parts(parts, Body::from(bytes)))
            }
            Err(_) => {
                return super::api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "请求体过大，无法登记操作编号",
                );
            }
        }
    } else {
        // multipart 等流式请求不预读请求体，仅按路由与查询参数登记
        let fp = crate::operations::fingerprint(
            &state,
            &identity.user_id,
            method.as_str(),
            &path,
            &query,
            None,
        );
        (fp, req)
    };

    match crate::operations::reserve(
        &state,
        &op_id,
        &identity.user_id,
        method.as_str(),
        &path,
        &fingerprint,
    ) {
        crate::operations::Reserve::Reserved => {
            let resp = next.run(req).await;
            let status = resp.status();
            let (parts, body) = resp.into_parts();
            let bytes = match axum::body::to_bytes(body, MAX_REPLAY_BODY).await {
                Ok(b) => b,
                Err(_) => Bytes::new(),
            };
            let value: Option<Value> = serde_json::from_slice(&bytes).ok();
            let job_id = value
                .as_ref()
                .and_then(|v| v.get("job_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            crate::operations::complete(&state, &op_id, status.as_u16(), value, job_id);
            Response::from_parts(parts, Body::from(bytes))
        }
        crate::operations::Reserve::Replay(op) => replay_response(&op),
        crate::operations::Reserve::Conflict => super::api_error(
            StatusCode::CONFLICT,
            "操作编号已用于不同的请求内容，请使用新的操作编号",
        ),
        crate::operations::Reserve::PrincipalConflict => {
            super::api_error(StatusCode::CONFLICT, "操作编号属于其他账户")
        }
    }
}

fn replay_response(op: &crate::operations::Operation) -> Response {
    match op.status.as_str() {
        "pending" => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": "pending", "operation_id": op.id })),
        )
            .into_response(),
        "interrupted" => super::api_error(
            StatusCode::CONFLICT,
            "上次操作因面板重启中断，结果待确认；请核实后再决定是否重试",
        ),
        "done" | "error" => {
            let status = StatusCode::from_u16(op.status_code).unwrap_or(StatusCode::OK);
            match &op.response {
                Some(v) => (status, Json(v.clone())).into_response(),
                None => (status, Json(json!({ "ok": op.status == "done" }))).into_response(),
            }
        }
        _ => super::api_error(StatusCode::CONFLICT, "操作状态未知"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::config::PanelConfig;
    use crate::instance::{InstanceMeta, InstanceRuntime};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use tower::ServiceExt;

    struct Harness {
        state: AppState,
        dir: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn setup() -> Harness {
        let dir = std::env::temp_dir().join(format!("mcspr-jobs-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::new(PanelConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            ..PanelConfig::default()
        })
        .await
        .unwrap();
        Harness { state, dir }
    }

    async fn add_instance(state: &AppState, id: &str) {
        let dir = state.config.read().await.instances_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = InstanceMeta {
            id: id.to_string(),
            name: format!("inst-{id}"),
            ..Default::default()
        };
        std::fs::write(dir.join("instance.json"), serde_json::to_string(&meta).unwrap()).unwrap();
        let rt = InstanceRuntime::new(meta, dir);
        state.instances.write().await.insert(id.to_string(), rt);
    }

    async fn create_user(h: &Harness, username: &str, role: Role, instances: &[&str]) -> String {
        h.state
            .auth
            .create_user(
                username,
                "password123",
                role,
                instances.iter().map(|s| s.to_string()).collect(),
            )
            .await
            .unwrap()
            .id
    }

    async fn login(h: &Harness, username: &str) -> (String, String) {
        let resp = crate::api::router(h.state.clone())
            .oneshot(
                req("POST", "/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "username": username, "password": "password123" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        (cookie, body["csrf_token"].as_str().unwrap().to_string())
    }

    async fn call(h: &Harness, request: Request<Body>) -> (StatusCode, Value) {
        let resp = crate::api::router(h.state.clone()).oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    fn req(method: &str, uri: &str) -> axum::http::request::Builder {
        Request::builder().method(method).uri(uri)
    }

    fn create_body(name: &str, cookie: &str, csrf: &str, op_id: &str) -> Request<Body> {
        req("POST", "/api/instances")
            .header("content-type", "application/json")
            .header(header::COOKIE, cookie)
            .header("x-csrf-token", csrf)
            .header("x-operation-id", op_id)
            .body(Body::from(json!({ "name": name }).to_string()))
            .unwrap()
    }

    async fn instance_count(h: &Harness, cookie: &str) -> usize {
        let (_, body) = call(
            h,
            req("GET", "/api/instances")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        body["instances"].as_array().map(|a| a.len()).unwrap_or(0)
    }

    #[tokio::test]
    async fn duplicate_operation_id_replays_single_effect() {
        let h = setup().await;
        create_user(&h, "root", Role::Admin, &[]).await;
        let (cookie, csrf) = login(&h, "root").await;

        let (s1, b1) = call(&h, create_body("dup", &cookie, &csrf, "op-dup")).await;
        assert_eq!(s1, StatusCode::OK, "{b1}");
        let (s2, b2) = call(&h, create_body("dup", &cookie, &csrf, "op-dup")).await;
        assert_eq!(s2, StatusCode::OK, "{b2}");
        assert_eq!(b1["id"], b2["id"], "重放应返回同一结果");
        assert_eq!(instance_count(&h, &cookie).await, 1, "只应产生一次副作用");
    }

    #[tokio::test]
    async fn concurrent_duplicate_requests_create_once() {
        let h = setup().await;
        create_user(&h, "root", Role::Admin, &[]).await;
        let (cookie, csrf) = login(&h, "root").await;
        let r1 = crate::api::router(h.state.clone())
            .oneshot(create_body("race", &cookie, &csrf, "op-race"));
        let r2 = crate::api::router(h.state.clone())
            .oneshot(create_body("race", &cookie, &csrf, "op-race"));
        let (a, b) = tokio::join!(r1, r2);
        let mut ids = Vec::new();
        for resp in [a.unwrap(), b.unwrap()] {
            let status = resp.status();
            assert!(status.is_success(), "并发请求应成功或处理中: {status}");
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(id) = v.get("id").and_then(|v| v.as_str()) {
                    ids.push(id.to_string());
                }
            }
        }
        if ids.len() == 2 {
            assert_eq!(ids[0], ids[1], "并发重放应返回同一实例");
        }
        assert_eq!(instance_count(&h, &cookie).await, 1, "并发只应创建一次");
    }

    #[tokio::test]
    async fn same_operation_id_different_body_conflicts() {
        let h = setup().await;
        create_user(&h, "root", Role::Admin, &[]).await;
        let (cookie, csrf) = login(&h, "root").await;
        let (s1, _) = call(&h, create_body("first", &cookie, &csrf, "op-x")).await;
        assert_eq!(s1, StatusCode::OK);
        let (s2, body) = call(&h, create_body("second", &cookie, &csrf, "op-x")).await;
        assert_eq!(s2, StatusCode::CONFLICT, "{body}");
        assert_eq!(instance_count(&h, &cookie).await, 1);
    }

    #[tokio::test]
    async fn operation_status_lookup_returns_result() {
        let h = setup().await;
        create_user(&h, "root", Role::Admin, &[]).await;
        let (cookie, csrf) = login(&h, "root").await;
        let (_, created) = call(&h, create_body("lookup", &cookie, &csrf, "op-lookup")).await;
        let (status, op) = call(
            &h,
            req("GET", "/api/operations/op-lookup")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(op["status"], "done");
        assert_eq!(op["result"]["id"], created["id"]);
    }

    #[tokio::test]
    async fn task_list_and_detail_filter_by_role() {
        let h = setup().await;
        let admin = create_user(&h, "root", Role::Admin, &[]).await;
        let alice = create_user(&h, "alice", Role::User, &[]).await;
        let admin_job = crate::jobs::create_job(
            &h.state,
            crate::jobs::NewJob {
                kind: "java-install".into(),
                title: "admin job".into(),
                user_id: Some(admin.clone()),
                ..Default::default()
            },
        );
        let alice_job = crate::jobs::create_job(
            &h.state,
            crate::jobs::NewJob {
                kind: "modpack-import".into(),
                title: "alice job".into(),
                user_id: Some(alice.clone()),
                ..Default::default()
            },
        );

        let (admin_cookie, _) = login(&h, "root").await;
        let (_, admin_list) = call(
            &h,
            req("GET", "/api/jobs")
                .header(header::COOKIE, &admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(admin_list["total"], 2, "管理员可见全部任务");

        let (alice_cookie, _) = login(&h, "alice").await;
        let (_, alice_list) = call(
            &h,
            req("GET", "/api/jobs")
                .header(header::COOKIE, &alice_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(alice_list["total"], 1, "普通用户仅见自己的任务");
        assert_eq!(alice_list["jobs"][0]["id"], alice_job);

        let (status, _) = call(
            &h,
            req("GET", &format!("/api/jobs/{admin_job}"))
                .header(header::COOKIE, &alice_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "不可读取他人任务");
    }

    fn multipart_field(body: &mut Vec<u8>, boundary: &str, name: &str, filename: &str, data: &[u8]) {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }

    #[tokio::test]
    async fn mods_upload_stages_files_and_reports_partial_failures() {
        let h = setup().await;
        create_user(&h, "root", Role::Admin, &[]).await;
        add_instance(&h.state, "test").await;
        let (cookie, csrf) = login(&h, "root").await;

        let boundary = "X-BOUNDARY-STAGE";
        let mut body = Vec::new();
        multipart_field(&mut body, boundary, "file", "good.jar", b"JARDATA");
        multipart_field(&mut body, boundary, "file", "bad.txt", b"TEXT");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let request = req("POST", "/api/instances/test/mods/upload")
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .body(Body::from(body))
            .unwrap();
        let (status, val) = call(&h, request).await;
        assert_eq!(status, StatusCode::OK, "{val}");
        assert_eq!(val["saved"], json!(["good.jar"]));
        assert_eq!(val["failed"][0]["name"], "bad.txt");

        let mods_dir = h
            .state
            .config
            .read()
            .await
            .instances_dir()
            .join("test")
            .join("mods");
        assert!(mods_dir.join("good.jar").exists());
        let leftovers: Vec<String> = std::fs::read_dir(&mods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".upload-"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");
    }
}
