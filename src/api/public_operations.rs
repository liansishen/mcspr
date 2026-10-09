use crate::operations::{self, Reserve, ReserveError};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

// 注册写请求的指纹包含凭据；持久化记录仅保存 HMAC 与安全响应。
pub async fn middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if req.method() != axum::http::Method::POST
        || !matches!(
            path.strip_prefix("/api").unwrap_or(&path),
            "/auth/register" | "/auth/application/resubmit"
        )
    {
        return next.run(req).await;
    }
    if !super::origin_ok(req.headers()) {
        return super::api_error(StatusCode::FORBIDDEN, "请求来源不受信任");
    }
    let Some(id) = req
        .headers()
        .get("x-operation-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| operations::is_valid_id(s))
        .map(str::to_owned)
    else {
        return next.run(req).await;
    };
    let query = req.uri().query().unwrap_or("").to_owned();
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 8 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return super::api_error(StatusCode::PAYLOAD_TOO_LARGE, "请求体过大"),
    };
    let principal = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
        v.get("username")
            .and_then(Value::as_str)
            .map(crate::auth::normalize_username)
    });
    let req = Request::from_parts(parts, Body::from(bytes.clone()));
    let Some(username) = principal else {
        return next.run(req).await;
    };
    let principal = format!("applicant:{username}");
    let fingerprint =
        operations::fingerprint(&state, &principal, "POST", &path, &query, Some(&bytes));
    match operations::reserve(&state, &id, &principal, "POST", &path, &fingerprint) {
        Ok(Reserve::Reserved) => {
            let worker = tokio::spawn(async move {
                let response = next.run(req).await;
                let status = response.status();
                let actor = response
                    .extensions()
                    .get::<crate::audit::AuditActor>()
                    .cloned();
                let target = super::audit_target(&path);
                crate::audit::record(
                    &state,
                    "POST",
                    &path,
                    status.as_u16(),
                    actor.as_ref(),
                    target.as_deref(),
                )
                .await;
                let (parts, body) = response.into_parts();
                let bytes = axum::body::to_bytes(body, 64 * 1024)
                    .await
                    .unwrap_or_default();
                let result = serde_json::from_slice(&bytes).ok();
                operations::complete(&state, &id, status.as_u16(), result, None);
                let mut response = Response::from_parts(parts, Body::from(bytes));
                response
                    .extensions_mut()
                    .insert(super::jobs::OperationAudited);
                response
            });
            worker.await.unwrap_or_else(|_| {
                super::api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "操作结果待确认，请查询申请状态",
                )
            })
        }
        Ok(Reserve::Replay(op)) if op.status == "pending" => (
            StatusCode::ACCEPTED,
            Json(json!({"status":"pending", "operation_id":op.id})),
        )
            .into_response(),
        Ok(Reserve::Replay(op)) if matches!(op.status.as_str(), "done" | "error") => {
            let status =
                StatusCode::from_u16(op.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(op.response.unwrap_or(json!({"ok":false})))).into_response()
        }
        Ok(Reserve::Replay(_)) => {
            super::api_error(StatusCode::CONFLICT, "上次提交结果待确认，请先查询申请状态")
        }
        Ok(Reserve::Conflict | Reserve::PrincipalConflict) => {
            super::api_error(StatusCode::CONFLICT, "操作编号已用于其他请求内容")
        }
        Err(ReserveError::Storage(error)) => {
            tracing::error!(%error, "注册操作登记失败");
            super::api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "申请登记暂不可用，请稍后重试",
            )
        }
        Err(ReserveError::Capacity) => super::api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "正在处理的请求过多，请稍后重试",
        ),
    }
}
