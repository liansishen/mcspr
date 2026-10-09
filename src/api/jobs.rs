//! 任务中心与写操作幂等：任务列表 / 详情 / 重试、操作状态查询与幂等中间件。

use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::jobs::Job;
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

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

/// 失败任务重试：当前没有可安全自动重放的任务类型，返回手动重试指引。
/// 对失败 / 中断任务，解除其关联操作编号的幂等记录，使该编号可重新执行；
/// 已完成的任务不会解除（沿用旧编号会回放上次结果，应改用新编号）。
pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<Value>> {
    let job = find_job(&state, &id)?;
    if !identity.is_admin() && job.user_id.as_deref() != Some(identity.user_id.as_str()) {
        return Err(ApiError::not_found("任务不存在"));
    }
    let mut released = false;
    if matches!(job.status.as_str(), "error" | "interrupted") {
        if let Some(op) = crate::operations::find_by_job(&state, &job.id) {
            if identity.is_admin() || op.principal == identity.user_id {
                released = crate::operations::forget(&state, &op.id);
            }
        }
    }
    Ok(Json(json!({
        "ok": false,
        "manual": true,
        "retryable": false,
        "reuse_operation_id": released,
        "kind": job.kind,
        "message": if released {
            "该任务不支持自动重试；已解除旧操作编号的幂等记录，可沿用原编号重新发起，或使用新编号"
        } else {
            "该任务不支持自动重试；请重新发起操作并使用新的 X-Operation-ID（沿用已完成/未知的旧编号会回放上次结果）"
        },
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
        "operation_id": job.operation_id,
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
    if t.len() > 2 && t.contains('\\') {
        return true;
    }
    // Unix 绝对路径
    if t.starts_with('/') && t.len() > 1 {
        return true;
    }
    // Windows 盘符路径 C:/...
    let b = t.as_bytes();
    t.len() > 2 && b[1] == b':' && b[2] == b'/'
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
/// 3. JSON 按原始字节指纹；multipart 先落盘并解析出「逻辑指纹」
///    （字段名 / 文件名 / 长度 / 内容 SHA-256），因此重放要求逻辑内容一致，
///    不要求 multipart boundary 一致；不同内容一律 409；
/// 4. 首次请求在副作用前登记，处理在独立任务中执行，客户端断连不影响结果登记。
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
    let mime = ctype.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let is_json = mime == "application/json";
    let is_multipart = mime == "multipart/form-data";

    let (fingerprint, req, spool) = if is_json {
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
                (fp, Request::from_parts(parts, Body::from(bytes)), None)
            }
            Err(_) => {
                return super::api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "请求体过大，无法登记操作编号",
                );
            }
        }
    } else if is_multipart {
        // multipart 需先落盘才能得到稳定内容指纹（不能只按路由登记，否则同编号不同文件会被静默重放）
        let (parts, body) = req.into_parts();
        let (spooled, logical) = match spool_multipart(&state, &ctype, body).await {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };
        let fp = crate::operations::fingerprint_digest(
            &state,
            &identity.user_id,
            method.as_str(),
            &path,
            &query,
            Some(&logical),
        );
        let (body, path_guard) = match spooled {
            Spooled::Memory(b) => (Body::from(b), None),
            Spooled::File(p) => {
                let file = match tokio::fs::File::open(&p).await {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&p).await;
                        return super::api_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("读取上传缓存失败: {e}"),
                        );
                    }
                };
                (
                    Body::from_stream(tokio_util::io::ReaderStream::new(file)),
                    Some(p),
                )
            }
        };
        (fp, Request::from_parts(parts, body), path_guard)
    } else {
        // 其它请求（含无 Content-Type）：读取实际正文并按原始字节指纹，空正文也参与哈希
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
                (fp, Request::from_parts(parts, Body::from(bytes)), None)
            }
            Err(_) => {
                return super::api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "请求体过大，无法登记操作编号",
                );
            }
        }
    };

    match crate::operations::reserve(
        &state,
        &op_id,
        &identity.user_id,
        method.as_str(),
        &path,
        &fingerprint,
    ) {
        Ok(crate::operations::Reserve::Reserved) => {
            // 统一任务中心：为本次写操作登记任务（尽力而为，失败不阻断请求）
            let (kind, title, inst) = op_meta(&method, &path);
            let op_job = match crate::jobs::create_job(
                &state,
                crate::jobs::NewJob {
                    kind,
                    title,
                    instance_id: inst,
                    user_id: Some(identity.user_id.clone()),
                    operation_id: Some(op_id.clone()),
                },
            ) {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!("操作任务登记失败: {e}");
                    None
                }
            };
            // 在独立任务中执行：客户端断连、请求 future 被丢弃时仍会完成并登记结果
            let task_state = state.clone();
            let task_op = op_id.clone();
            let principal = identity.user_id.clone();
            let audit = AuditContext {
                method: method.as_str().to_string(),
                path: path.clone(),
                actor: crate::audit::AuditActor::from_identity(&identity),
            };
            let handle = tokio::spawn(async move {
                execute_and_record(task_state, task_op, op_job, principal, audit, spool, next.run(req))
                    .await
            });
            match handle.await {
                Ok(resp) => resp,
                Err(_) => super::api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "操作执行任务异常终止",
                ),
            }
        }
        Ok(crate::operations::Reserve::Replay(op)) => {
            remove_spool(spool).await;
            replay_response(&op)
        }
        Ok(crate::operations::Reserve::Conflict) => {
            remove_spool(spool).await;
            super::api_error(
                StatusCode::CONFLICT,
                "操作编号已用于不同的请求内容，请使用新的操作编号",
            )
        }
        Ok(crate::operations::Reserve::PrincipalConflict) => {
            remove_spool(spool).await;
            super::api_error(StatusCode::CONFLICT, "操作编号属于其他账户")
        }
        Err(crate::operations::ReserveError::Storage(e)) => {
            remove_spool(spool).await;
            super::api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("操作登记持久化失败，未执行任何变更，请稍后重试（{e}）"),
            )
        }
        Err(crate::operations::ReserveError::Capacity) => {
            remove_spool(spool).await;
            super::api_error(StatusCode::SERVICE_UNAVAILABLE, "操作登记已满，请稍后重试")
        }
    }
}

/// 审计上下文：在已接受操作的任务内记录一次，外层 audit_mw 通过 marker 跳过。
struct AuditContext {
    method: String,
    path: String,
    actor: crate::audit::AuditActor,
}

/// 幂等层内部已完成审计的响应标记，供 audit_mw 去重。
#[derive(Clone)]
pub struct OperationAudited;

/// 根据方法与路径推导操作任务类型 / 中文标题 / 关联实例。
fn op_meta(method: &Method, path: &str) -> (String, String, Option<String>) {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let segs = if segs.first() == Some(&"api") {
        &segs[1..]
    } else {
        &segs[..]
    };
    let head = segs.first().copied().unwrap_or("");
    let is_import = head == "instances" && segs.get(1) == Some(&"import");
    let instance = if head == "instances" && !is_import {
        segs.get(1).map(|s| s.to_string()).filter(|s| !s.is_empty())
    } else {
        None
    };
    if is_import {
        return ("modpack-import".to_string(), "导入实例".to_string(), None);
    }
    let sub = segs.get(2).copied().unwrap_or("");
    let (kind, title) = match (head, sub) {
        ("instances", "") => {
            if method == Method::POST {
                ("instance-create", "创建实例")
            } else {
                ("instance-update", "修改实例设置")
            }
        }
        ("instances", "start") => ("instance-power", "启动实例"),
        ("instances", "stop") => ("instance-power", "停止实例"),
        ("instances", "restart") => ("instance-power", "重启实例"),
        ("instances", "command") => ("console-command", "发送控制台命令"),
        ("instances", "eula") => ("instance-eula", "同意 EULA"),
        ("instances", "backups") => ("backup", "备份操作"),
        ("instances", "game-backups") => ("game-backup", "游戏内备份操作"),
        ("instances", "clone") => ("instance-clone", "克隆实例"),
        ("instances", "reinstall") => ("instance-reinstall", "重装实例"),
        ("instances", "modpack") => ("modpack", "整合包操作"),
        ("instances", "mods") => ("mod-op", "模组操作"),
        ("instances", "files") => ("file-op", "文件操作"),
        ("instances", "worlds") => ("world-op", "世界操作"),
        ("instances", "properties") => ("instance-props", "实例属性"),
        ("instances", "icon") => ("instance-icon", "实例图标"),
        ("instances", "tasks") => ("scheduled-task", "计划任务"),
        ("instances", "announcement") => ("announcement", "保存公告"),
        ("instances", "configs") => ("instance-config", "实例配置"),
        ("instances", "users") => ("instance-users", "玩家管理"),
        ("instances", _) => ("instance-op", "实例操作"),
        ("accounts", _) => ("account", "账户操作"),
        ("settings", _) => ("panel-settings", "面板设置"),
        ("config", _) => ("config-io", "配置导入导出"),
        ("javas", _) | ("java-install", _) => ("java-install", "Java 安装/扫描"),
        ("moddb", _) => ("moddb", "模组市场操作"),
        ("announcements", _) => ("announcement", "公告操作"),
        _ => ("write", "写操作"),
    };
    (kind.to_string(), title.to_string(), instance)
}

/// 执行已登记的操作并把结果写回登记表；即使外层请求被取消也会运行到结束。
async fn execute_and_record(
    state: AppState,
    op_id: String,
    op_job: Option<String>,
    principal: String,
    audit: AuditContext,
    spool: Option<PathBuf>,
    fut: impl std::future::Future<Output = Response>,
) -> Response {
    use futures_util::FutureExt;
    let resp = match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(_) => super::api_error(StatusCode::INTERNAL_SERVER_ERROR, "操作执行异常终止"),
    };
    remove_spool(spool).await;
    let status = resp.status();
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_REPLAY_BODY).await {
        Ok(b) => b,
        Err(_) => {
            // 响应体过大：登记状态但无法缓存响应体用于重放
            crate::operations::complete(&state, &op_id, status.as_u16(), None, None);
            if let Some(oj) = &op_job {
                crate::jobs::finish_operation_job(
                    &state,
                    oj,
                    false,
                    None,
                    Some("响应体过大，无法缓存操作结果".into()),
                );
            }
            return super::api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "响应体过大，无法缓存操作结果",
            );
        }
    };
    let value: Option<Value> = serde_json::from_slice(&bytes).ok();
    let job_id = value
        .as_ref()
        .and_then(|v| v.get("job_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 统一任务中心：关联后台任务，并完成本次操作任务
    if let Some(oj) = &op_job {
        match &job_id {
            Some(bg) => {
                crate::jobs::attach_operation(&state, bg, &op_id, &principal);
                crate::jobs::finish_operation_job(
                    &state,
                    oj,
                    true,
                    Some(json!({ "note": "已提交后台任务", "background_job_id": bg })),
                    None,
                );
            }
            None => {
                let ok = status.as_u16() < 400;
                let err = value
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                crate::jobs::finish_operation_job(
                    &state,
                    oj,
                    ok,
                    Some(json!({ "status_code": status.as_u16() })),
                    err,
                );
            }
        }
    }

    crate::operations::complete(&state, &op_id, status.as_u16(), value, job_id);

    // 审计：在已接受操作的独立任务内记录一次；即使客户端断连也会写入
    crate::audit::record(
        &state,
        &audit.method,
        &audit.path,
        status.as_u16(),
        Some(&audit.actor),
        super::audit_target(&audit.path).as_deref(),
    )
    .await;
    parts.extensions.insert(OperationAudited);
    Response::from_parts(parts, Body::from(bytes))
}

async fn remove_spool(spool: Option<PathBuf>) {
    if let Some(p) = spool {
        let _ = tokio::fs::remove_file(p).await;
    }
}

/// 已读取的请求体：小请求驻留内存，大请求落盘；二者都可重新作为 Body 交给处理器。
enum Spooled {
    Memory(Bytes),
    File(PathBuf),
}

/// 内存驻留阈值；超过则落盘到 `data/tasks/upload-*.spool`
const SPOOL_MEMORY_LIMIT: usize = 8 * 1024 * 1024;
/// 落盘上限（与 DefaultBodyLimit 一致）
const MAX_SPOOL_BODY: usize = 1024 * 1024 * 1024;

/// 读取 multipart 请求体并解析稳定的逻辑指纹：
/// 每个字段按 `name\0filename\0len\0content-sha256` 参与指纹，
/// 因此相同逻辑文件（即使 boundary 不同）可重放，内容不同则冲突。
async fn spool_multipart(
    state: &AppState,
    ctype: &str,
    body: Body,
) -> Result<(Spooled, String), ApiError> {
    let boundary = multer::parse_boundary(ctype)
        .map_err(|e| ApiError::bad_request(format!("multipart boundary 无效: {e}")))?;
    let mut stream = body.into_data_stream();
    let mut mem: Vec<u8> = Vec::new();
    let mut file: Option<tokio::fs::File> = None;
    let mut spool_path: Option<PathBuf> = None;
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                return Err(cleanup_err(
                    &mut spool_path,
                    ApiError::bad_request(format!("读取上传数据失败: {e}")),
                )
                .await)
            }
        };
        total += chunk.len();
        if total > MAX_SPOOL_BODY {
            return Err(cleanup_err(
                &mut spool_path,
                ApiError::bad_request("上传数据超过大小上限"),
            )
            .await);
        }
        if let Some(f) = file.as_mut() {
            if let Err(e) = f.write_all(&chunk).await {
                return Err(cleanup_err(
                    &mut spool_path,
                    ApiError::internal(format!("写入上传缓存失败: {e}")),
                )
                .await);
            }
        } else {
            mem.extend_from_slice(&chunk);
            if mem.len() > SPOOL_MEMORY_LIMIT {
                let path = state
                    .tasks_dir
                    .join(format!("upload-{}.spool", uuid::Uuid::new_v4()));
                let mut f = match tokio::fs::File::create(&path).await {
                    Ok(f) => f,
                    Err(e) => {
                        return Err(ApiError::internal(format!("创建上传缓存失败: {e}")))
                    }
                };
                if let Err(e) = f.write_all(&mem).await {
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(ApiError::internal(format!("写入上传缓存失败: {e}")));
                }
                mem.clear();
                spool_path = Some(path);
                file = Some(f);
            }
        }
    }
    if let Some(f) = file.as_mut() {
        if let Err(e) = f.flush().await {
            return Err(cleanup_err(
                &mut spool_path,
                ApiError::internal(format!("写入上传缓存失败: {e}")),
            )
            .await);
        }
    }
    let logical = if let Some(path) = spool_path.as_ref() {
        let stream = match tokio::fs::File::open(path).await {
            Ok(f) => tokio_util::io::ReaderStream::new(f),
            Err(e) => {
                return Err(cleanup_err(
                    &mut spool_path,
                    ApiError::internal(format!("读取上传缓存失败: {e}")),
                )
                .await)
            }
        };
        match multipart_logical_fingerprint(stream, boundary).await {
            Ok(v) => v,
            Err(e) => return Err(cleanup_err(&mut spool_path, e).await),
        }
    } else {
        let bytes = Bytes::from(mem.clone());
        let stream = futures_util::stream::once(async move {
            Ok::<Bytes, std::io::Error>(bytes)
        });
        multipart_logical_fingerprint(stream, boundary).await?
    };
    let spooled = match spool_path {
        Some(path) => Spooled::File(path),
        None => Spooled::Memory(Bytes::from(mem)),
    };
    Ok((spooled, logical))
}

/// 出错时清理已落盘的上传缓存，避免泄漏临时文件。
async fn cleanup_err(spool_path: &mut Option<PathBuf>, err: ApiError) -> ApiError {
    if let Some(p) = spool_path.take() {
        let _ = tokio::fs::remove_file(p).await;
    }
    err
}

async fn multipart_logical_fingerprint<S>(
    stream: S,
    boundary: String,
) -> Result<String, ApiError>
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let mut mp = multer::Multipart::new(stream, boundary);
    let mut canonical = String::new();
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("解析上传数据失败: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().unwrap_or("").to_string();
        let mut hasher = Sha256::new();
        let mut len = 0usize;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| ApiError::bad_request(format!("读取上传字段失败: {e}")))?
        {
            hasher.update(&chunk);
            len += chunk.len();
        }
        canonical.push_str(&name);
        canonical.push('\0');
        canonical.push_str(&filename);
        canonical.push('\0');
        canonical.push_str(&len.to_string());
        canonical.push('\0');
        canonical.push_str(&crate::operations::sha256_hex(&hasher.finalize()));
        canonical.push('\n');
    }
    Ok(canonical)
}

fn replay_response(op: &crate::operations::Operation) -> Response {
    match op.status.as_str() {
        "pending" => {
            let poll = format!("/api/operations/{}", op.id);
            let mut resp = (
                StatusCode::ACCEPTED,
                Json(json!({ "status": "pending", "operation_id": op.id, "poll": poll })),
            )
                .into_response();
            if let Ok(v) = HeaderValue::from_str(&poll) {
                resp.headers_mut().insert(header::LOCATION, v);
            }
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            resp
        }
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
        )
        .unwrap();
        let alice_job = crate::jobs::create_job(
            &h.state,
            crate::jobs::NewJob {
                kind: "modpack-import".into(),
                title: "alice job".into(),
                user_id: Some(alice.clone()),
                ..Default::default()
            },
        )
        .unwrap();

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

    fn mods_upload_req(
        cookie: &str,
        csrf: &str,
        boundary: &str,
        filename: &str,
        data: &[u8],
    ) -> Request<Body> {
        let mut body = Vec::new();
        multipart_field(&mut body, boundary, "file", filename, data);
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        req("POST", "/api/instances/test/mods/upload")
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .header(header::COOKIE, cookie)
            .header("x-csrf-token", csrf)
            .header("x-operation-id", "op-mp")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn multipart_idempotency_uses_logical_content_not_boundary() {
        let h = setup().await;
        create_user(&h, "root", Role::Admin, &[]).await;
        add_instance(&h.state, "test").await;
        let (cookie, csrf) = login(&h, "root").await;

        // 相同逻辑文件、不同 boundary：应重放，而不是静默当成新上传
        let (s1, b1) = call(&h, mods_upload_req(&cookie, &csrf, "BOUND-A", "a.jar", b"DATA1")).await;
        assert_eq!(s1, StatusCode::OK, "{b1}");
        assert_eq!(b1["saved"], json!(["a.jar"]));
        let (s2, b2) = call(&h, mods_upload_req(&cookie, &csrf, "BOUND-B", "a.jar", b"DATA1")).await;
        assert_eq!(s2, StatusCode::OK, "{b2}");
        assert_eq!(b2["saved"], json!(["a.jar"]));

        // 相同编号、不同内容：冲突
        let (s3, b3) = call(&h, mods_upload_req(&cookie, &csrf, "BOUND-C", "a.jar", b"DIFFERENT")).await;
        assert_eq!(s3, StatusCode::CONFLICT, "{b3}");

        // 无残留临时文件
        let mods_dir = h
            .state
            .config
            .read()
            .await
            .instances_dir()
            .join("test")
            .join("mods");
        let leftovers: Vec<String> = std::fs::read_dir(&mods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".upload-") || n.starts_with("upload-"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");
    }

    #[tokio::test]
    async fn accepted_operation_completes_when_outer_request_dropped() {
        let h = setup().await;
        let fp = crate::operations::fingerprint(&h.state, "u1", "POST", "/api/slow", "", Some(b"{}"));
        let _ = crate::operations::reserve(&h.state, "op-drop", "u1", "POST", "/api/slow", &fp).unwrap();
        let handle = tokio::spawn(execute_and_record(
            h.state.clone(),
            "op-drop".into(),
            None,
            "u1".into(),
            AuditContext {
                method: "POST".into(),
                path: "/api/slow".into(),
                actor: crate::audit::AuditActor::default(),
            },
            None,
            async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
            },
        ));
        // 模拟客户端断连：丢弃外层等待句柄，任务应继续运行并登记结果
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let op = crate::operations::lookup(&h.state, "op-drop", "u1", false).unwrap();
        assert_eq!(op.status, "done");
        assert_eq!(op.response.unwrap()["ok"], true);
    }

    #[tokio::test]
    async fn retry_releases_failed_job_operation() {
        let h = setup().await;
        let admin = create_user(&h, "root", Role::Admin, &[]).await;
        let job = crate::jobs::create_job(
            &h.state,
            crate::jobs::NewJob {
                kind: "modpack-update".into(),
                title: "更新".into(),
                user_id: Some(admin.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        crate::jobs::finish_job(&h.state, &job, Some("boom".into()), None);
        let fp = crate::operations::fingerprint(&h.state, &admin, "POST", "/api/x", "", Some(b"{}"));
        let _ = crate::operations::reserve(&h.state, "op-retry", &admin, "POST", "/api/x", &fp).unwrap();
        crate::operations::complete(
            &h.state,
            "op-retry",
            400,
            Some(json!({ "error": "boom" })),
            Some(job.clone()),
        );

        let (cookie, csrf) = login(&h, "root").await;
        let (status, body) = call(
            &h,
            req("POST", &format!("/api/jobs/{job}/retry"))
                .header(header::COOKIE, &cookie)
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["manual"], true);
        assert_eq!(body["reuse_operation_id"], true, "失败任务的旧操作编号应被解除");
        assert!(
            crate::operations::lookup(&h.state, "op-retry", &admin, true).is_none(),
            "旧编号应可重新执行"
        );
    }

    #[tokio::test]
    async fn write_operation_records_task_metadata() {
        let h = setup().await;
        let admin = create_user(&h, "root", Role::Admin, &[]).await;
        add_instance(&h.state, "inst-a").await;
        let (cookie, csrf) = login(&h, "root").await;

        // 无正文的写操作（无 Content-Type）：应读取空正文并登记操作任务
        let (status, body) = call(
            &h,
            req("POST", "/api/instances/inst-a/eula")
                .header(header::COOKIE, &cookie)
                .header("x-csrf-token", &csrf)
                .header("x-operation-id", "op-meta-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(status.is_success(), "{body}");

        let (_, list) = call(
            &h,
            req("GET", "/api/jobs")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let jobs = list["jobs"].as_array().unwrap();
        let job = jobs
            .iter()
            .find(|j| j["operation_id"] == "op-meta-1")
            .expect("写操作应登记任务中心记录");
        assert_eq!(job["kind"], "instance-eula");
        assert_eq!(job["title"], "同意 EULA");
        assert_eq!(job["instance_id"], "inst-a");
        assert_eq!(job["user_id"], admin);
        assert_eq!(job["status"], "done");
    }
}
