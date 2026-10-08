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
    std::fs::write(dir.join("instance.json"), serde_json::to_string(&meta).unwrap()).unwrap();
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
    let (status, _) = call(&h, req("GET", "/api/instances").body(Body::empty()).unwrap()).await;
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
            .body(Body::from(json!({ "username": "root", "password": "bad" }).to_string()))
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
        ("PATCH", format!("/api/accounts/{admin_id}"), json!({ "enabled": false })),
        ("PATCH", format!("/api/accounts/{admin_id}"), json!({ "role": "user" })),
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
        assert_eq!(status, StatusCode::BAD_REQUEST, "应保护最后一个管理员: {method} {uri}");
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
            .body(Body::from(json!({ "instance_ids": ["inst-a"] }).to_string()))
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
            .body(Body::from(json!({ "password": "newpassword1" }).to_string()))
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
            .body(Body::from(json!({
                "username": format!("missing-{attempt}"), "password": "password123"
            }).to_string()))
            .unwrap();
        let (status, _) = call(&h, request).await;
        assert_eq!(status, if attempt < 30 {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        });
    }
}
