//! Cloudflare Turnstile 人机验证（Siteverify 服务端校验）。
//!
//! 注册与重申共用 [`verify`]；正式模式调用官方 Siteverify 校验成功标志、主机名与操作用途，
//! 配置缺失时**失败关闭**（拒绝而非放行）。测试模式必须显式开启，且只接受官方测试密钥与
//! 官方测试令牌，避免在正式接口中接受任意伪造令牌。
//!
//! 官方依据：
//! - https://developers.cloudflare.com/turnstile/get-started/server-side-validation/
//! - https://developers.cloudflare.com/turnstile/troubleshooting/testing/

use crate::config::PanelConfig;
use serde::Deserialize;
use std::time::Duration;

/// Siteverify 校验端点
pub const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// 官方“始终通过”测试站点密钥
pub const TEST_SITE_KEY: &str = "1x00000000000000000000AA";
/// 官方“始终通过”测试服务端密钥
pub const TEST_SECRET_KEY: &str = "1x0000000000000000000000000000000AA";
/// 官方“始终失败”测试服务端密钥
pub const TEST_FAIL_SECRET_KEY: &str = "2x0000000000000000000000000000000AA";
/// 官方“令牌已使用”测试服务端密钥
pub const TEST_SPENT_SECRET_KEY: &str = "3x0000000000000000000000000000000AA";
/// 官方测试站点密钥生成的固定测试令牌
pub const TEST_PASS_TOKEN: &str = "XXXX.DUMMY.TOKEN.XXXX";

/// Siteverify 外部请求超时
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// 人机验证配置快照（从面板设置读取，避免持有配置锁跨越网络请求）。
#[derive(Debug, Clone, Default)]
pub struct TurnstileSettings {
    pub site_key: String,
    pub secret_key: String,
    pub allowed_hostnames: Vec<String>,
    pub test_mode: bool,
}

impl TurnstileSettings {
    pub fn from_config(c: &PanelConfig) -> Self {
        Self {
            site_key: c.turnstile_site_key.clone(),
            secret_key: c.turnstile_secret_key.clone(),
            allowed_hostnames: c.turnstile_allowed_hostnames.clone(),
            test_mode: c.turnstile_test_mode,
        }
    }

    /// 校验配置是否足以开启注册（测试模式使用官方密钥即可）。
    pub fn is_configured(&self) -> Result<(), String> {
        if self.test_mode {
            if !self.site_key.is_empty() && self.site_key != TEST_SITE_KEY {
                return Err("测试模式必须使用官方测试站点密钥".to_string());
            }
            if !self.secret_key.is_empty() && self.secret_key != TEST_SECRET_KEY {
                return Err("测试模式必须使用官方测试服务端密钥".to_string());
            }
            return Ok(());
        }
        if self.secret_key.trim().is_empty() {
            return Err("未配置 Turnstile 服务端密钥".to_string());
        }
        if is_test_secret(&self.secret_key) {
            return Err("正式模式不能使用官方测试密钥".to_string());
        }
        if self.site_key.trim().is_empty() {
            return Err("未配置 Turnstile 站点密钥".to_string());
        }
        if self.allowed_hostnames.is_empty() {
            return Err("未配置 Turnstile 允许的主机名".to_string());
        }
        Ok(())
    }
}

fn is_test_secret(secret: &str) -> bool {
    matches!(
        secret,
        TEST_SECRET_KEY | TEST_FAIL_SECRET_KEY | TEST_SPENT_SECRET_KEY
    )
}

/// 校验失败原因；由调用方映射为合适的 HTTP 状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptchaError {
    /// 配置缺失或非法：正式模式必须失败关闭（503）
    Config(String),
    /// 令牌缺失、无效、过期、重放或主机名/用途不匹配（400）
    Failed(String),
    /// Siteverify 不可达（503）
    Unavailable(String),
}

impl CaptchaError {
    pub fn message(&self) -> &str {
        match self {
            CaptchaError::Config(m) | CaptchaError::Failed(m) | CaptchaError::Unavailable(m) => m,
        }
    }
}

/// Siteverify 响应
#[derive(Debug, Clone, Deserialize)]
pub struct SiteverifyResponse {
    pub success: bool,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default, rename = "error-codes")]
    pub error_codes: Vec<String>,
}

/// 解释 Siteverify 响应（纯函数，便于离线测试主机名/用途/重放错误）。
pub fn interpret(
    resp: &SiteverifyResponse,
    expected_action: &str,
    allowed_hostnames: &[String],
) -> Result<(), CaptchaError> {
    if !resp.success {
        let detail = if resp.error_codes.is_empty() {
            "人机验证失败".to_string()
        } else {
            format!("人机验证失败: {}", resp.error_codes.join(","))
        };
        return Err(CaptchaError::Failed(detail));
    }
    match resp.action.as_deref() {
        Some(action) if action == expected_action => {}
        Some(action) => {
            return Err(CaptchaError::Failed(format!(
                "人机验证操作用途不匹配（期望 {expected_action}，实际 {action}）"
            )));
        }
        None => return Err(CaptchaError::Failed("人机验证缺少操作用途".to_string())),
    }
    let Some(hostname) = resp.hostname.as_deref() else {
        return Err(CaptchaError::Failed("人机验证缺少主机名".to_string()));
    };
    if !allowed_hostnames
        .iter()
        .any(|h| h.trim().eq_ignore_ascii_case(hostname))
    {
        return Err(CaptchaError::Failed(
            "人机验证主机名不在允许列表".to_string(),
        ));
    }
    Ok(())
}

/// 校验一次 Turnstile 令牌。
///
/// `expected_action` 为前端组件设置的用途（注册与重申统一使用 `register`）。
/// 生产环境通过 Siteverify 校验并强制主机名/用途匹配；测试模式仅接受官方测试令牌。
pub async fn verify(
    http: &reqwest::Client,
    settings: &TurnstileSettings,
    token: &str,
    remote_ip: Option<&str>,
    expected_action: &str,
) -> Result<(), CaptchaError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(CaptchaError::Failed("缺少人机验证令牌".to_string()));
    }
    settings.is_configured().map_err(CaptchaError::Config)?;

    if settings.test_mode {
        // 测试模式：只接受官方测试令牌，避免在常规接口中接受任意伪造令牌。
        if token == TEST_PASS_TOKEN {
            return Ok(());
        }
        return Err(CaptchaError::Failed(
            "测试模式仅接受官方测试令牌".to_string(),
        ));
    }

    let mut form: Vec<(&str, &str)> = vec![
        ("secret", settings.secret_key.as_str()),
        ("response", token),
    ];
    if let Some(ip) = remote_ip.filter(|s| !s.trim().is_empty()) {
        form.push(("remoteip", ip));
    }
    let resp = http
        .post(SITEVERIFY_URL)
        .timeout(VERIFY_TIMEOUT)
        .form(&form)
        .send()
        .await
        .map_err(|e| CaptchaError::Unavailable(format!("人机验证服务不可达: {e}")))?;
    if !resp.status().is_success() {
        return Err(CaptchaError::Unavailable(format!(
            "人机验证服务返回异常状态: {}",
            resp.status()
        )));
    }
    let parsed: SiteverifyResponse = resp
        .json()
        .await
        .map_err(|e| CaptchaError::Unavailable(format!("人机验证响应解析失败: {e}")))?;
    interpret(&parsed, expected_action, &settings.allowed_hostnames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(test_mode: bool, secret: &str, hostnames: &[&str]) -> TurnstileSettings {
        TurnstileSettings {
            site_key: if test_mode {
                TEST_SITE_KEY.to_string()
            } else {
                "0x4AAAAAAAsitekey".to_string()
            },
            secret_key: secret.to_string(),
            allowed_hostnames: hostnames.iter().map(|s| s.to_string()).collect(),
            test_mode,
        }
    }

    #[test]
    fn production_missing_config_fails_closed() {
        assert!(settings(false, "", &["panel.local"])
            .is_configured()
            .is_err());
        assert!(settings(false, "0x4secret", &[]).is_configured().is_err());
        assert!(settings(false, "0x4secret", &["panel.local"])
            .is_configured()
            .is_ok());
    }

    #[test]
    fn production_rejects_official_test_keys() {
        let s = settings(false, TEST_SECRET_KEY, &["panel.local"]);
        assert!(s.is_configured().is_err());
    }

    #[test]
    fn test_mode_requires_official_keys() {
        assert!(settings(true, TEST_SECRET_KEY, &[]).is_configured().is_ok());
        assert!(settings(true, "0x4custom", &[]).is_configured().is_err());
    }

    #[tokio::test]
    async fn test_mode_accepts_only_official_token() {
        let http = reqwest::Client::new();
        let s = settings(true, TEST_SECRET_KEY, &[]);
        assert!(verify(&http, &s, TEST_PASS_TOKEN, None, "register")
            .await
            .is_ok());
        assert!(verify(&http, &s, "fake-token", None, "register")
            .await
            .is_err());
        assert!(verify(&http, &s, "", None, "register").await.is_err());
    }

    #[test]
    fn interpret_enforces_action_and_hostname() {
        let allowed = vec!["panel.local".to_string()];
        let ok = SiteverifyResponse {
            success: true,
            hostname: Some("Panel.Local".to_string()),
            action: Some("register".to_string()),
            error_codes: vec![],
        };
        assert!(interpret(&ok, "register", &allowed).is_ok());

        let wrong_action = SiteverifyResponse {
            action: Some("login".to_string()),
            ..ok.clone()
        };
        assert!(interpret(&wrong_action, "register", &allowed).is_err());

        let wrong_host = SiteverifyResponse {
            hostname: Some("evil.example".to_string()),
            ..ok.clone()
        };
        assert!(interpret(&wrong_host, "register", &allowed).is_err());

        let missing_host = SiteverifyResponse {
            hostname: None,
            ..ok.clone()
        };
        assert!(interpret(&missing_host, "register", &allowed).is_err());
    }

    #[test]
    fn interpret_reports_expired_and_replayed_tokens() {
        let allowed = vec!["panel.local".to_string()];
        let expired = SiteverifyResponse {
            success: false,
            hostname: Some("panel.local".to_string()),
            action: Some("register".to_string()),
            error_codes: vec!["timeout-or-duplicate".to_string()],
        };
        let err = interpret(&expired, "register", &allowed).unwrap_err();
        assert!(err.message().contains("timeout-or-duplicate"));
    }
}
