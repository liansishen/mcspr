//! 认证接口：登录、退出、当前身份、修改自己的密码。

use crate::audit::AuditActor;
use crate::auth::Identity;
use crate::captcha;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

/// 实际对端地址（来自 `ConnectInfo`，即 `into_make_service_with_connect_info` 注入）。
///
/// 不使用 `X-Forwarded-For` / `X-Real-IP` 等可伪造头；无连接信息时返回 `None`。
pub struct PeerAddr(pub Option<std::net::SocketAddr>);

impl<S> axum::extract::FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0),
        ))
    }
}

#[derive(Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

/// 登录：原子预占限流额度（先于密码校验），统一失败提示，成功下发 HttpOnly 会话 Cookie。
pub async fn login(
    State(state): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Response {
    let attempted = crate::auth::normalize_username(&req.username);
    if !super::origin_ok(&headers) {
        return super::api_error(StatusCode::FORBIDDEN, "请求来源不受信任");
    }
    let account_key = format!("u:{attempted}");
    let source_key = format!(
        "s:{}",
        peer.map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    // 先占额度再校验密码：并发尝试也无法绕过限流
    if !state.auth.reserve_login(&account_key, &source_key).await {
        return super::api_error_actor(
            StatusCode::TOO_MANY_REQUESTS,
            "登录尝试过于频繁，请稍后再试",
            AuditActor {
                user_id: None,
                username: Some(attempted),
                role: None,
            },
        );
    }
    let Some((user, epoch)) = state
        .auth
        .verify_credentials(&req.username, &req.password)
        .await
    else {
        return super::api_error_actor(
            StatusCode::UNAUTHORIZED,
            "用户名或密码错误",
            AuditActor {
                user_id: None,
                username: Some(attempted),
                role: None,
            },
        );
    };
    if user.status != crate::auth::AccountStatus::Approved {
        return super::api_error_actor(
            StatusCode::FORBIDDEN,
            "账户尚未通过审核",
            AuditActor {
                user_id: Some(user.id.clone()),
                username: Some(user.username.clone()),
                role: Some(user.role.as_str().to_string()),
            },
        );
    }
    // 凭据纪元必须与校验时一致，避免重置竞态下旧密码拿到新会话
    let Some((token, csrf)) = state.auth.start_session(&user.id, epoch).await else {
        return super::api_error_actor(
            StatusCode::UNAUTHORIZED,
            "账户凭据已变更，请重试",
            AuditActor {
                user_id: Some(user.id.clone()),
                username: Some(user.username.clone()),
                role: Some(user.role.as_str().to_string()),
            },
        );
    };
    state.auth.login_succeeded(&account_key).await;
    let secure = super::is_secure_request(&headers);
    let cookie = super::session_cookie(&token, secure, crate::auth::SESSION_TTL.as_secs() as i64);
    let actor = AuditActor {
        user_id: Some(user.id.clone()),
        username: Some(user.username.clone()),
        role: Some(user.role.as_str().to_string()),
    };
    let mut resp = (
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "user": user, "csrf_token": csrf })),
    )
        .into_response();
    resp.extensions_mut().insert(actor);
    resp
}

/// 当前登录身份与 CSRF token（前端启动时调用）。
pub async fn me(Extension(identity): Extension<Identity>) -> Json<serde_json::Value> {
    Json(json!({
        "user": {
            "id": identity.user_id,
            "username": identity.username,
            "role": identity.role,
            "enabled": true,
            "instance_ids": identity.instance_ids,
        },
        "csrf_token": identity.csrf,
    }))
}

/// 退出：撤销当前会话并清除 Cookie。
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Extension(identity): Extension<Identity>,
) -> Response {
    state.auth.revoke_session(&identity.session).await;
    let cookie = super::session_cookie("", super::is_secure_request(&headers), 0);
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

#[derive(Deserialize)]
pub struct PasswordReq {
    pub old_password: String,
    pub password: String,
}

/// 修改自己的密码；成功后提升会话纪元，撤销所有旧会话（含当前）。
pub async fn password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Extension(identity): Extension<Identity>,
    Json(req): Json<PasswordReq>,
) -> Response {
    match state
        .auth
        .change_password(&identity.user_id, &req.old_password, &req.password)
        .await
    {
        Ok(()) => {
            let cookie = super::session_cookie("", super::is_secure_request(&headers), 0);
            ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
        }
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

/// 来源键：仅使用真实对端 IP，忽略可伪造的转发头。
fn peer_key(peer: Option<std::net::SocketAddr>) -> String {
    peer.map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 将人机验证错误映射为 HTTP 响应：配置/不可达失败关闭，校验失败返回 400。
fn captcha_error(e: captcha::CaptchaError) -> Response {
    let status = match e {
        captcha::CaptchaError::Config(_) | captcha::CaptchaError::Unavailable(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        captcha::CaptchaError::Failed(_) => StatusCode::BAD_REQUEST,
    };
    super::api_error(status, e.message().to_string())
}

/// 公开注册配置：只返回站点密钥与开关，绝不返回服务端密钥。
pub async fn registration_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let c = state.config.read().await;
    Json(json!({
        "enabled": c.registration_enabled,
        "site_key": c.turnstile_site_key,
        "test_mode": c.turnstile_test_mode,
    }))
}

#[derive(Deserialize)]
pub struct RegisterReq {
    pub username: String,
    pub password: String,
    pub minecraft_name: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub captcha_token: String,
}

/// 注册：始终创建待审批的普通用户；本地校验先于一次性验证令牌消费。
pub async fn register(
    State(state): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(req): Json<RegisterReq>,
) -> Response {
    if !super::origin_ok(&headers) {
        return super::api_error(StatusCode::FORBIDDEN, "请求来源不受信任");
    }
    let (enabled, settings) = {
        let c = state.config.read().await;
        (
            c.registration_enabled,
            captcha::TurnstileSettings::from_config(&c),
        )
    };
    if !enabled {
        return super::api_error(StatusCode::FORBIDDEN, "注册暂未开放");
    }
    // 未创建管理员时不允许注册：pending 申请不得占用存储，避免首次管理员 CLI 被锁死。
    if !state.auth.has_users().await {
        return super::api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "面板尚未完成初始化，请先创建管理员",
        );
    }
    let attempted = crate::auth::normalize_username(&req.username);
    let account_key = format!("reg:u:{attempted}");
    let src = format!("reg:s:{}", peer_key(peer));
    if !state.auth.reserve_public(&account_key, &src).await {
        return super::api_error(StatusCode::TOO_MANY_REQUESTS, "操作过于频繁，请稍后再试");
    }
    if let Err(e) = crate::auth::validate_username(&attempted) {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    if let Err(e) = crate::auth::validate_password(&req.password) {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    if let Err(e) = crate::auth::normalize_minecraft_name(&req.minecraft_name) {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    let ip = peer.map(|a| a.ip().to_string());
    if let Err(e) = captcha::verify(
        &state.http,
        &settings,
        &req.captcha_token,
        ip.as_deref(),
        "register",
    )
    .await
    {
        return captcha_error(e);
    }
    match state
        .auth
        .register_pending_user(
            &req.username,
            &req.password,
            &req.minecraft_name,
            &req.reason,
        )
        .await
    {
        Ok(_) => Json(json!({ "ok": true, "status": "pending" })).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
pub struct ApplicationStatusReq {
    pub username: String,
    pub password: String,
}

/// 申请状态查询：凭据校验，不创建业务会话，响应禁止缓存。
pub async fn application_status(
    State(state): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(req): Json<ApplicationStatusReq>,
) -> Response {
    if !super::origin_ok(&headers) {
        return super::api_error(StatusCode::FORBIDDEN, "请求来源不受信任");
    }
    let attempted = crate::auth::normalize_username(&req.username);
    let account_key = format!("u:{attempted}");
    let src = format!("s:{}", peer_key(peer));
    if !state.auth.reserve_login(&account_key, &src).await {
        return super::api_error(StatusCode::TOO_MANY_REQUESTS, "操作过于频繁，请稍后再试");
    }
    let Some((user, _epoch)) = state
        .auth
        .verify_credentials(&req.username, &req.password)
        .await
    else {
        return super::api_error_actor(
            StatusCode::UNAUTHORIZED,
            "用户名或密码错误",
            AuditActor {
                user_id: None,
                username: Some(attempted),
                role: None,
            },
        );
    };
    let app = state.auth.application_for_user(&user.id).await;
    let mut resp = Json(json!({ "application": app })).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Deserialize)]
pub struct ResubmitReq {
    pub username: String,
    pub password: String,
    pub minecraft_name: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub captcha_token: String,
}

/// 被拒绝申请重申：凭据校验 + 人机验证，复用注册的验证流程。
pub async fn application_resubmit(
    State(state): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(req): Json<ResubmitReq>,
) -> Response {
    if !super::origin_ok(&headers) {
        return super::api_error(StatusCode::FORBIDDEN, "请求来源不受信任");
    }
    let settings = {
        let c = state.config.read().await;
        captcha::TurnstileSettings::from_config(&c)
    };
    let attempted = crate::auth::normalize_username(&req.username);
    let account_key = format!("u:{attempted}");
    let src = format!("s:{}", peer_key(peer));
    if !state.auth.reserve_login(&account_key, &src).await {
        return super::api_error(StatusCode::TOO_MANY_REQUESTS, "操作过于频繁，请稍后再试");
    }
    let Some((user, _epoch)) = state
        .auth
        .verify_credentials(&req.username, &req.password)
        .await
    else {
        return super::api_error_actor(
            StatusCode::UNAUTHORIZED,
            "用户名或密码错误",
            AuditActor {
                user_id: None,
                username: Some(attempted),
                role: None,
            },
        );
    };
    if user.status != crate::auth::AccountStatus::Rejected {
        return super::api_error(StatusCode::BAD_REQUEST, "当前申请无需重新提交");
    }
    if let Err(e) = crate::auth::normalize_minecraft_name(&req.minecraft_name) {
        return super::api_error(StatusCode::BAD_REQUEST, e);
    }
    let ip = peer.map(|a| a.ip().to_string());
    if let Err(e) = captcha::verify(
        &state.http,
        &settings,
        &req.captcha_token,
        ip.as_deref(),
        "register",
    )
    .await
    {
        return captcha_error(e);
    }
    match state
        .auth
        .resubmit_application(&user.id, &req.minecraft_name, &req.reason)
        .await
    {
        Ok(()) => Json(json!({ "ok": true, "status": "pending" })).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

/// 个人资料：返回本人信息与当前申请（含本人可见的申请理由）。
pub async fn profile(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    let Some(user) = state.auth.get(&identity.user_id).await else {
        return super::api_error(StatusCode::NOT_FOUND, "账户不存在");
    };
    let app = state.auth.application_for_user(&identity.user_id).await;
    Json(json!({
        "user": {
            "id": user.id,
            "username": user.username,
            "role": user.role,
            "enabled": user.enabled,
            "instance_ids": user.instance_ids,
            "minecraft_name": user.minecraft_name,
            "status": user.status,
        },
        "application": app,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct NameChangeReq {
    pub minecraft_name: String,
    #[serde(default)]
    pub reason: String,
}

/// 提交游戏名变更申请（首次绑定游戏名同样走此流程）。
pub async fn create_name_request(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Json(req): Json<NameChangeReq>,
) -> Response {
    match state
        .auth
        .request_name_change(&identity.user_id, &req.minecraft_name, &req.reason)
        .await
    {
        Ok(r) => (StatusCode::CREATED, Json(json!({ "request": r }))).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}

/// 撤回自己的待处理游戏名变更申请。
pub async fn delete_name_request(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Response {
    match state
        .auth
        .withdraw_name_change(&identity.user_id, &id)
        .await
    {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}
