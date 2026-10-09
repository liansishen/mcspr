//! 完整路由级权限测试：通过 tower 的 `oneshot` 请求真实 Router，覆盖中间件与授权。

use super::*;
use crate::auth::Role;
use crate::config::PanelConfig;
use crate::instance::{InstanceMeta, InstanceRuntime};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Harness {
    state: AppState,
    dir: std::path::PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn setup(instances: &[&str]) -> Harness {
    let dir = std::env::temp_dir().join(format!("mcspr_api_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = PanelConfig {
        data_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    };
    let state = AppState::new(cfg).await.unwrap();
    for id in instances {
        add_instance(&state, id).await;
    }
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
    std::fs::write(
        dir.join("instance.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();
    let rt = InstanceRuntime::new(meta, dir);
    state.instances.write().await.insert(id.to_string(), rt);
}

async fn call(h: &Harness, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router(h.state.clone()).oneshot(req).await.unwrap();
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

async fn login(h: &Harness, username: &str, password: &str) -> (String, String) {
    let resp = router(h.state.clone())
        .oneshot(
            req("POST", "/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": username, "password": password }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
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
    assert_eq!(status, StatusCode::OK, "登录失败: {body}");
    (cookie, body["csrf_token"].as_str().unwrap().to_string())
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

// ---------- ping / 初始化 ----------

#[tokio::test]
async fn ping_reports_auth_required_and_initialized() {
    let h = setup(&[]).await;
    let (status, body) = call(&h, req("GET", "/api/ping").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["auth_required"], true);
    assert_eq!(body["initialized"], false);

    create_user(&h, "root", Role::Admin, &[]).await;
    let (_, body) = call(&h, req("GET", "/api/ping").body(Body::empty()).unwrap()).await;
    assert_eq!(body["initialized"], true);
}

#[tokio::test]
async fn uninitialized_api_is_closed() {
    let h = setup(&[]).await;
    let (status, _) = call(
        &h,
        req("GET", "/api/instances").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------- 登录 / 身份 ----------

#[tokio::test]
async fn login_me_and_wrong_password() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;

    let (status, body) = call(
        &h,
        req("POST", "/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "username": "root", "password": "bad" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());

    let (cookie, csrf) = login(&h, "root", "password123").await;
    let (status, body) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["username"], "root");
    assert_eq!(body["user"]["role"], "admin");
    assert_eq!(body["csrf_token"], csrf);
}

#[tokio::test]
async fn legacy_token_never_grants_access() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    h.state.config.write().await.token = "legacy-token".to_string();
    let (status, _) = call(
        &h,
        req("GET", "/api/instances")
            .header(header::AUTHORIZATION, "Bearer legacy-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------- 实例列表 / 详情权限 ----------

#[tokio::test]
async fn ordinary_user_sees_only_authorized_safe_instances() {
    let h = setup(&["inst-a", "inst-b"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    create_user(&h, "alice", Role::User, &["inst-a"]).await;

    let (cookie, _) = login(&h, "alice", "password123").await;

    let (status, body) = call(
        &h,
        req("GET", "/api/instances")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = body["instances"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "inst-a");
    assert!(list[0].get("pid").is_none());
    assert!(list[0].get("java_path").is_none());
    assert!(list[0].get("jvm_args").is_none());

    // 管理员列表包含全部实例与完整字段
    let (cookie, _) = login(&h, "root", "password123").await;
    let (_, body) = call(
        &h,
        req("GET", "/api/instances")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["instances"].as_array().unwrap().len(), 2);
    assert!(body["instances"][0].get("pid").is_some());
}

#[tokio::test]
async fn ordinary_user_cannot_read_unauthorized_instance() {
    let h = setup(&["inst-a", "inst-b"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    create_user(&h, "alice", Role::User, &["inst-a"]).await;
    let (cookie, _) = login(&h, "alice", "password123").await;

    for uri in [
        "/api/instances/inst-b",
        "/api/instances/inst-b/status",
        "/api/instances/inst-b/overview",
        "/api/instances/inst-b/playtime",
        "/api/instances/inst-b/announcement",
    ] {
        let (status, _) = call(
            &h,
            req("GET", uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "应拒绝未授权实例: {uri}");
    }

    let (status, _) = call(
        &h,
        req("GET", "/api/instances/inst-a")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn ordinary_user_blocked_from_admin_routes() {
    let h = setup(&["inst-a"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    create_user(&h, "alice", Role::User, &["inst-a"]).await;
    let (cookie, csrf) = login(&h, "alice", "password123").await;

    for (method, uri) in [
        ("GET", "/api/accounts"),
        ("POST", "/api/accounts"),
        ("GET", "/api/stats"),
        ("GET", "/api/settings"),
        ("GET", "/api/audit"),
        ("GET", "/api/instances/inst-a/mods"),
        ("GET", "/api/instances/inst-a/console"),
        ("GET", "/api/instances/inst-a/ws"),
        ("POST", "/api/instances/inst-a/start"),
        ("PUT", "/api/instances/inst-a/announcement"),
        ("POST", "/api/announcements/preview"),
    ] {
        let (status, _) = call(
            &h,
            req(method, uri)
                .header(header::COOKIE, &cookie)
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "普通用户应被拒绝: {method} {uri}"
        );
    }
}

// ---------- CSRF / Origin ----------

#[tokio::test]
async fn mutating_requests_require_csrf() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;

    let (status, _) = call(
        &h,
        req("POST", "/api/auth/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(
        &h,
        req("POST", "/api/auth/logout")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn origin_mismatch_is_rejected() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let (cookie, _) = login(&h, "root", "password123").await;

    let (status, _) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, "http://evil.example")
            .header(header::HOST, "panel.local")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(
        &h,
        req("POST", "/api/auth/login")
            .header("content-type", "application/json")
            .header(header::ORIGIN, "http://evil.example")
            .header(header::HOST, "panel.local")
            .body(Body::from(
                json!({ "username": "root", "password": "password123" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------- 账户管理 ----------

#[tokio::test]
async fn accounts_admin_crud_and_last_admin_protection() {
    let h = setup(&["inst-a"]).await;
    let admin_id = create_user(&h, "root", Role::Admin, &[]).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;

    let (status, body) = call(
        &h,
        req("GET", "/api/accounts")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["accounts"].as_array().unwrap().len(), 1);

    let (status, body) = call(
        &h,
        req("POST", "/api/accounts")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "username": "bob", "password": "password123", "role": "user", "instance_ids": ["inst-a"] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let bob_id = body["account"]["id"].as_str().unwrap().to_string();

    // 最后一个可用管理员不可禁用 / 删除 / 降级
    for (method, uri, payload) in [
        (
            "PATCH",
            format!("/api/accounts/{admin_id}"),
            json!({ "enabled": false }),
        ),
        (
            "PATCH",
            format!("/api/accounts/{admin_id}"),
            json!({ "role": "user" }),
        ),
        ("DELETE", format!("/api/accounts/{admin_id}"), json!({})),
    ] {
        let (status, _) = call(
            &h,
            req(method, &uri)
                .header(header::COOKIE, &cookie)
                .header("x-csrf-token", &csrf)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "应保护最后一个管理员: {method} {uri}"
        );
    }

    // 无效实例 ID 被拒绝
    let (status, _) = call(
        &h,
        req("PUT", &format!("/api/accounts/{bob_id}/instances"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(json!({ "instance_ids": ["nope"] }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 有效授权保存成功
    let (status, body) = call(
        &h,
        req("PUT", &format!("/api/accounts/{bob_id}/instances"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "instance_ids": ["inst-a"] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["account"]["instance_ids"][0], "inst-a");
}

#[tokio::test]
async fn session_revoked_on_disable_reset_and_role_change() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let uid = create_user(&h, "bob", Role::User, &[]).await;
    let (admin_cookie, admin_csrf) = login(&h, "root", "password123").await;

    // 禁用撤销会话
    let (bob_cookie, _) = login(&h, "bob", "password123").await;
    let (status, _) = call(
        &h,
        req("PATCH", &format!("/api/accounts/{uid}"))
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(json!({ "enabled": false }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 重新启用后重置密码撤销会话
    let (status, _) = call(
        &h,
        req("PATCH", &format!("/api/accounts/{uid}"))
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(json!({ "enabled": true }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (bob_cookie, _) = login(&h, "bob", "password123").await;
    let (status, _) = call(
        &h,
        req("PUT", &format!("/api/accounts/{uid}/password"))
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "password": "newpassword1" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 角色变更撤销会话
    let (bob_cookie, _) = login(&h, "bob", "newpassword1").await;
    let (status, _) = call(
        &h,
        req("PATCH", &format!("/api/accounts/{uid}"))
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(json!({ "role": "admin" }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------- 自身密码 / 退出 ----------

#[tokio::test]
async fn change_own_password_and_logout_revoke_sessions() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;

    let (status, _) = call(
        &h,
        req("PUT", "/api/auth/password")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "old_password": "password123", "password": "changedpass1" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 新密码可登录
    let (cookie, csrf) = login(&h, "root", "changedpass1").await;
    let (status, _) = call(
        &h,
        req("POST", "/api/auth/logout")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &h,
        req("GET", "/api/auth/me")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------- 公告 / 运行信息 ----------

#[tokio::test]
async fn announcement_write_is_admin_only_and_read_is_authorized() {
    let h = setup(&["inst-a"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    create_user(&h, "alice", Role::User, &["inst-a"]).await;
    let (admin_cookie, admin_csrf) = login(&h, "root", "password123").await;
    let (alice_cookie, _) = login(&h, "alice", "password123").await;

    let (status, _) = call(
        &h,
        req("PUT", "/api/instances/inst-a/announcement")
            .header(header::COOKIE, &alice_cookie)
            .body(Body::from(
                json!({ "markdown": "# hi", "expected_updated_at": "" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = call(
        &h,
        req("PUT", "/api/instances/inst-a/announcement")
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "markdown": "# 公告", "expected_updated_at": "" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["markdown"], "# 公告");

    let (status, body) = call(
        &h,
        req("GET", "/api/instances/inst-a/announcement")
            .header(header::COOKIE, &alice_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["markdown"], "# 公告");
    assert!(body["html"].as_str().unwrap().contains("公告"));

    let (status, body) = call(
        &h,
        req("GET", "/api/instances/inst-a/overview")
            .header(header::COOKIE, &alice_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "inst-a");
    assert!(body["player_stats"].is_array());
}

// ---------- WebSocket 握手 ----------

#[tokio::test]
async fn websocket_handshake_is_admin_only() {
    let h = setup(&["inst-a"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    create_user(&h, "alice", Role::User, &["inst-a"]).await;
    let (admin_cookie, _) = login(&h, "root", "password123").await;
    let (alice_cookie, _) = login(&h, "alice", "password123").await;

    let ws_req = |cookie: Option<&str>| {
        let mut b = req("GET", "/api/instances/inst-a/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    };

    let (status, _) = call(&h, ws_req(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call(&h, ws_req(Some(&alice_cookie))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 管理员通过鉴权：tower 单测没有真实 hyper 升级句柄，无法完成握手（426），
    // 关键是未被鉴权中间件拦截。
    let (status, _) = call(&h, ws_req(Some(&admin_cookie))).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn audit_records_actor_and_target() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;
    let bob = create_user(&h, "bob", Role::User, &[]).await;

    let (status, _) = call(
        &h,
        req("PATCH", &format!("/api/accounts/{bob}"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(json!({ "enabled": false }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &h,
        req("GET", "/api/audit")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["target"] == format!("accounts:{bob}"))
        .expect("审计应记录账户变更目标");
    assert_eq!(entry["user"], "root");
    assert_eq!(entry["role"], "admin");
}

#[tokio::test]
async fn denied_request_audit_records_actor() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    create_user(&h, "alice", Role::User, &[]).await;
    let (alice_cookie, _) = login(&h, "alice", "password123").await;

    // 普通用户访问管理员接口 -> 403，审计应记录已解析的操作者
    let (status, _) = call(
        &h,
        req("GET", "/api/accounts")
            .header(header::COOKIE, &alice_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (admin_cookie, _) = login(&h, "root", "password123").await;
    let (status, body) = call(
        &h,
        req("GET", "/api/audit?q=alice")
            .header(header::COOKIE, &admin_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["status"] == 403 && e["user"] == "alice")
        .expect("被拒请求应记录操作者");
    assert_eq!(entry["role"], "user");
    assert!(entry["user_id"].is_string(), "审计应包含 user_id");
}

#[tokio::test]
async fn login_failure_audit_records_attempted_username() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    // 不存在的账户 + 错误密码
    let (status, _) = call(
        &h,
        req("POST", "/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "username": "Ghost", "password": "wrongpass" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (admin_cookie, _) = login(&h, "root", "password123").await;
    let (_, body) = call(
        &h,
        req("GET", "/api/audit?q=ghost")
            .header(header::COOKIE, &admin_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let entries = body["entries"].as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["status"] == 401 && e["user"] == "ghost")
        .expect("登录失败应记录尝试的用户名");
    assert!(entry["role"].is_null(), "登录失败不记录角色");
}

#[tokio::test]
async fn websocket_closes_after_admin_session_revoked() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    let h = setup(&["inst-a"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    // 第二个管理员：允许禁用 root
    create_user(&h, "root2", Role::Admin, &[]).await;
    let (cookie, _) = login(&h, "root", "password123").await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(h.state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let handshake = format!(
        "GET /api/instances/inst-a/ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: {cookie}\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).await.unwrap();
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await.unwrap();
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    assert!(
        String::from_utf8_lossy(&header).starts_with("HTTP/1.1 101"),
        "握手应升级成功"
    );
    let (opcode, _) = read_ws_frame(&mut stream).await.expect("应收到历史帧");
    assert_eq!(opcode, 0x1);

    // 撤销 root 会话（提升纪元），服务端应在一个周期内关闭连接
    let root_id = h.state.auth.find_by_username("root").await.unwrap().id;
    h.state.auth.set_enabled(&root_id, false).await.unwrap();

    let closed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match read_ws_frame(&mut stream).await {
                Some((0x8, _)) => return true,
                Some(_) => continue,
                None => return true,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "会话撤销后 WebSocket 应关闭");

    server.abort();
    let _ = server.await;
}

async fn read_ws_frame(stream: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).await.ok()?;
    let opcode = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b).await.ok()?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b).await.ok()?;
        len = u64::from_be_bytes(b) as usize;
    }
    if masked {
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask).await.ok()?;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
    Some((opcode, payload))
}

#[tokio::test]
async fn login_source_limit_uses_ip_across_ports_and_ignores_forwarded_headers() {
    let h = setup(&[]).await;
    for attempt in 0..31 {
        let peer: std::net::SocketAddr = format!("127.0.0.1:{}", 12000 + attempt).parse().unwrap();
        let request = req("POST", "/api/auth/login")
            .header("content-type", "application/json")
            .header("x-forwarded-for", format!("192.0.2.{}", attempt + 1))
            .extension(axum::extract::ConnectInfo(peer))
            .body(Body::from(
                json!({
                    "username": format!("missing-{attempt}"), "password": "password123"
                })
                .to_string(),
            ))
            .unwrap();
        let (status, _) = call(&h, request).await;
        assert_eq!(
            status,
            if attempt < 30 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::TOO_MANY_REQUESTS
            }
        );
    }
}

// ---------- 注册 / 审批 / 游戏名变更 / 实例权限 ----------

async fn enable_test_registration(h: &Harness) {
    let mut c = h.state.config.write().await;
    c.registration_enabled = true;
    c.turnstile_test_mode = true;
    c.turnstile_site_key = crate::captcha::TEST_SITE_KEY.to_string();
    c.turnstile_secret_key = crate::captcha::TEST_SECRET_KEY.to_string();
}

async fn register_user(h: &Harness, username: &str, mc: &str, token: &str) -> (StatusCode, Value) {
    call(
        h,
        req("POST", "/api/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "username": username,
                    "password": "password123",
                    "minecraft_name": mc,
                    "reason": "申请",
                    "captcha_token": token,
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await
}

/// 测试结束时恢复或删除工作目录下的 config.toml，避免污染仓库。
struct ConfigFileGuard {
    path: std::path::PathBuf,
    backup: Option<Vec<u8>>,
}

impl ConfigFileGuard {
    fn new() -> Self {
        let path = crate::config::config_path();
        let backup = std::fs::read(&path).ok();
        Self { path, backup }
    }
}

impl Drop for ConfigFileGuard {
    fn drop(&mut self) {
        match &self.backup {
            Some(b) => {
                let _ = std::fs::write(&self.path, b);
            }
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[tokio::test]
async fn registration_flow_is_gated_and_creates_pending_user() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;

    let (status, body) = call(
        &h,
        req("GET", "/api/auth/registration-config")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["test_mode"], false);
    assert_eq!(body["site_key"], "");

    // 未开启注册
    let (status, _) = register_user(&h, "bob", "Bob", crate::captcha::TEST_PASS_TOKEN).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    enable_test_registration(&h).await;

    // 测试模式只接受官方测试令牌
    let (status, _) = register_user(&h, "bob", "Bob", "fake-token").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = register_user(&h, "Bob", "Bob", crate::captcha::TEST_PASS_TOKEN).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pending");

    // 用户名与游戏名（忽略大小写）唯一
    let (status, _) = register_user(&h, "bob", "Other", crate::captcha::TEST_PASS_TOKEN).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = register_user(&h, "carol", "bob", crate::captcha::TEST_PASS_TOKEN).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 待审批账户不能获得业务会话
    let (status, _) = call(
        &h,
        req("POST", "/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "username": "bob", "password": "password123" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 状态查询：无会话、禁止缓存
    let resp = router(h.state.clone())
        .oneshot(
            req("POST", "/api/auth/application/status")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": "bob", "password": "password123" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["application"]["status"], "pending");
    assert_eq!(body["application"]["kind"], "registration");
    assert_eq!(body["application"]["minecraft_name"], "Bob");
    assert!(body.get("csrf_token").is_none());

    // 错误密码统一 401
    let (status, _) = call(
        &h,
        req("POST", "/api/auth/application/status")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "username": "bob", "password": "wrongpass" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_approval_assigns_grants_and_unlocks_login() {
    let h = setup(&["inst-a"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    enable_test_registration(&h).await;
    register_user(&h, "bob", "Bob", crate::captcha::TEST_PASS_TOKEN).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;

    let (status, body) = call(
        &h,
        req("GET", "/api/applications")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let apps = body["applications"].as_array().unwrap();
    assert_eq!(apps.len(), 1);
    let id = apps[0]["id"].as_str().unwrap().to_string();
    let rev = apps[0]["revision"].as_u64().unwrap();
    assert_eq!(apps[0]["kind"], "registration");

    // 错误修订号 -> 409
    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{id}/approve"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev + 99, "instance_ids": ["inst-a"] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // 批准并分配授权
    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{id}/approve"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "instance_ids": ["inst-a"] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 现在可登录并看到授权实例
    let (bob_cookie, _) = login(&h, "bob", "password123").await;
    let (status, body) = call(
        &h,
        req("GET", "/api/instances")
            .header(header::COOKIE, &bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["instances"].as_array().unwrap().len(), 1);
    assert_eq!(body["instances"][0]["id"], "inst-a");

    // 账户仍为普通用户，未注入角色；账户列表不含申请理由
    let (_, body) = call(
        &h,
        req("GET", "/api/accounts")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let bob = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["username"] == "bob")
        .unwrap();
    assert_eq!(bob["role"], "user");
    assert_eq!(bob["status"], "approved");
    assert_eq!(bob["minecraft_name"], "Bob");
    assert!(bob.get("application_reason").is_none());
}

#[tokio::test]
async fn rejection_reason_required_and_resubmit_returns_to_pending() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    enable_test_registration(&h).await;
    register_user(&h, "carol", "Carol", crate::captcha::TEST_PASS_TOKEN).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;
    let (_, body) = call(
        &h,
        req("GET", "/api/applications")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let id = body["applications"][0]["id"].as_str().unwrap().to_string();
    let rev = body["applications"][0]["revision"].as_u64().unwrap();

    // 拒绝必须给理由
    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{id}/reject"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "reason": "" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{id}/reject"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "reason": "资料不全" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 被拒后仍可用旧密码查询并看到理由
    let (status, body) = call(
        &h,
        req("POST", "/api/auth/application/status")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "username": "carol", "password": "password123" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["application"]["status"], "rejected");
    assert_eq!(body["application"]["rejection_reason"], "资料不全");

    // 重申回到待审批
    let (status, body) = call(
        &h,
        req("POST", "/api/auth/application/resubmit")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "username": "carol",
                    "password": "password123",
                    "minecraft_name": "Carol2",
                    "reason": "补充资料",
                    "captcha_token": crate::captcha::TEST_PASS_TOKEN,
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pending");

    // 旧修订号批准失败
    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{id}/approve"))
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "instance_ids": [] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn name_change_request_keeps_login_and_updates_name_on_approval() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    enable_test_registration(&h).await;
    register_user(&h, "dave", "Dave", crate::captcha::TEST_PASS_TOKEN).await;
    let (admin_cookie, admin_csrf) = login(&h, "root", "password123").await;
    let (_, body) = call(
        &h,
        req("GET", "/api/applications")
            .header(header::COOKIE, &admin_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let uid = body["applications"][0]["id"].as_str().unwrap().to_string();
    let rev = body["applications"][0]["revision"].as_u64().unwrap();
    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{uid}/approve"))
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "instance_ids": [] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (dave_cookie, dave_csrf) = login(&h, "dave", "password123").await;

    // 提交改名申请
    let (status, body) = call(
        &h,
        req("POST", "/api/auth/minecraft-name-requests")
            .header(header::COOKIE, &dave_cookie)
            .header("x-csrf-token", &dave_csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "minecraft_name": "Neo", "reason": "改名" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let req_id = body["request"]["id"].as_str().unwrap().to_string();

    // profile 显示待处理申请
    let (_, body) = call(
        &h,
        req("GET", "/api/auth/profile")
            .header(header::COOKIE, &dave_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["user"]["username"], "dave");
    assert_eq!(body["application"]["kind"], "name_change");
    assert_eq!(body["application"]["minecraft_name"], "Neo");

    // 管理员批准
    let (status, _) = call(
        &h,
        req("POST", &format!("/api/applications/{req_id}/approve"))
            .header(header::COOKIE, &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": 1, "instance_ids": [] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 游戏名更新，登录用户名不变
    let (_, body) = call(
        &h,
        req("GET", "/api/auth/profile")
            .header(header::COOKIE, &dave_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["user"]["minecraft_name"], "Neo");
    assert_eq!(body["user"]["username"], "dave");
    login(&h, "dave", "password123").await;

    // 撤回待处理申请
    let (status, body) = call(
        &h,
        req("POST", "/api/auth/minecraft-name-requests")
            .header(header::COOKIE, &dave_cookie)
            .header("x-csrf-token", &dave_csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "minecraft_name": "Neo2", "reason": "x" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let rid = body["request"]["id"].as_str().unwrap().to_string();
    let (status, _) = call(
        &h,
        req(
            "DELETE",
            &format!("/api/auth/minecraft-name-requests/{rid}"),
        )
        .header(header::COOKIE, &dave_cookie)
        .header("x-csrf-token", &dave_csrf)
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn instance_permissions_are_scoped_and_revision_checked() {
    let h = setup(&["inst-a", "inst-b"]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let bob = create_user(&h, "bob", Role::User, &[]).await;
    create_user(&h, "carol", Role::User, &["inst-a"]).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;

    let (status, body) = call(
        &h,
        req("GET", "/api/instances/inst-a/permissions")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rev = body["revision"].as_u64().unwrap();
    assert_eq!(body["whitelist_enabled"], false);
    let users = body["users"].as_array().unwrap();
    assert_eq!(
        users.iter().find(|u| u["username"] == "carol").unwrap()["granted"],
        true
    );
    assert_eq!(
        users.iter().find(|u| u["username"] == "bob").unwrap()["granted"],
        false
    );

    // 授权 bob（carol 的 inst-a 授权被本次提交替换）
    let (status, body) = call(
        &h,
        req("PUT", "/api/instances/inst-a/permissions")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "user_ids": [bob] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["revision"].as_u64().unwrap() > rev);
    let users = body["users"].as_array().unwrap();
    assert_eq!(
        users.iter().find(|u| u["username"] == "bob").unwrap()["granted"],
        true
    );
    assert_eq!(
        users.iter().find(|u| u["username"] == "carol").unwrap()["granted"],
        false
    );

    // 陈旧修订号冲突
    let (status, _) = call(
        &h,
        req("PUT", "/api/instances/inst-a/permissions")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "revision": rev, "user_ids": [] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // 普通用户不能访问权限接口
    let (bob_cookie, _) = login(&h, "bob", "password123").await;
    let (status, _) = call(
        &h,
        req("GET", "/api/instances/inst-a/permissions")
            .header(header::COOKIE, &bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 未知实例
    let (status, _) = call(
        &h,
        req("GET", "/api/instances/nope/permissions")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_guard_registration_and_export_redacts_secret() {
    let _guard = ConfigFileGuard::new();
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    let (cookie, csrf) = login(&h, "root", "password123").await;

    // 未配置人机验证时不能开启注册
    let (status, _) = call(
        &h,
        req("PUT", "/api/settings")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "registration_enabled": true }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 测试模式 + 官方密钥可开启
    let (status, _) = call(
        &h,
        req("PUT", "/api/settings")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &csrf)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "registration_enabled": true,
                    "turnstile_test_mode": true,
                    "turnstile_site_key": crate::captcha::TEST_SITE_KEY,
                    "turnstile_secret_key": crate::captcha::TEST_SECRET_KEY,
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 设置读取：服务端密钥脱敏，仅回 *_set
    let (_, body) = call(
        &h,
        req("GET", "/api/settings")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["turnstile_secret_key"], "");
    assert_eq!(body["turnstile_secret_key_set"], true);
    assert_eq!(body["registration_enabled"], true);

    // 导出配置脱敏
    let (_, body) = call(
        &h,
        req("GET", "/api/config/export")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["settings"]["turnstile_secret_key"], "");
    assert_eq!(body["settings"]["token"], "");
    assert_eq!(
        body["settings"]["turnstile_site_key"],
        crate::captcha::TEST_SITE_KEY
    );
}

#[tokio::test]
async fn registration_operation_replays_before_captcha_and_checks_origin() {
    let h = setup(&[]).await;
    create_user(&h, "root", Role::Admin, &[]).await;
    {
        let mut config = h.state.config.write().await;
        config.registration_enabled = true;
        config.turnstile_test_mode = true;
    }
    let body = json!({"username":"applicant", "password":"password123", "minecraft_name":"Applicant", "reason":"Registration replay test", "captcha_token":crate::captcha::TEST_PASS_TOKEN});
    let build = |body: &Value, origin: &str| req("POST", "/api/auth/register")
        .header("host", "localhost")
        .header("origin", origin)
        .header("content-type", "application/json")
        .header("x-operation-id", "registration-replay-test")
        .body(Body::from(body.to_string())).unwrap();
    let (first, result) = call(&h, build(&body, "http://localhost")).await;
    assert_eq!(first, StatusCode::OK, "{result}");
    let (second, replay) = call(&h, build(&body, "http://localhost")).await;
    assert_eq!(second, first);
    assert_eq!(result, replay);
    assert_eq!(h.state.auth.list().await.len(), 2);
    let mut changed = body.clone();
    changed["reason"] = json!("Changed reason");
    assert_eq!(call(&h, build(&changed, "http://localhost")).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&h, build(&body, "http://untrusted.example")).await.0, StatusCode::FORBIDDEN);
    let persisted = std::fs::read_to_string(h.state.tasks_dir.join("operations.json")).unwrap();
    assert!(!persisted.contains("password123"));
    assert!(!persisted.contains(crate::captcha::TEST_PASS_TOKEN));
}
