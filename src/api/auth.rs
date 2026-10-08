//! 认证接口：登录、退出、当前身份、修改自己的密码。

use crate::audit::AuditActor;
use crate::auth::Identity;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
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
        peer.map(|a| a.ip().to_string()).unwrap_or_else(|| "unknown".to_string())
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
    let Some((user, epoch)) = state.auth.verify_credentials(&req.username, &req.password).await
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
    (
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
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
            (
                [(header::SET_COOKIE, cookie)],
                Json(json!({ "ok": true })),
            )
                .into_response()
        }
        Err(e) => super::api_error(StatusCode::BAD_REQUEST, e),
    }
}
