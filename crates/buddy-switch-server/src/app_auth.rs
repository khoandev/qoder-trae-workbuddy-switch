//! 管理面**内置登录**：一把来自环境变量的密钥 + HttpOnly 会话 Cookie。
//!
//! ## 为什么需要它
//!
//! `api_guard` 只挡 DNS rebinding 与 CSRF（Host / Origin），**不做身份认证**：
//! 谁把 `webui` 端口暴露出去，谁就能 `POST /api/accounts/export` 拿走全部明文凭据。
//! 反向代理是**可选项**，Docker 用户常常直接映射端口 —— 于是这里给一个**内置**的、
//! 不依赖外部组件的门。
//!
//! ## 设计取舍
//!
//! - **密钥只来自环境变量**（[`AUTH_KEY_ENV`]），不落盘、不进配置面板：管理面登录的
//!   密钥若能在 webui 里改，攻击者拿到会话后就能改掉它，等于没有。
//! - **未设置 / 空串 = 关闭**：既有部署（`buddy-switch serve`、桌面端）行为一个字不变。
//!   这是刻意的 —— 静默开启登录会让所有现网用户在升级后突然进不去。
//! - **会话在内存里**（TTL [`SESSION_TTL_SECS`]）：重启服务 = 全部下线。用 HMAC 签名的
//!   无状态 Cookie 可以让会话活过重启，但那要求把签名密钥也持久化，带来新的落盘密钥；
//!   本功能的目标是「挡住陌生人」，不是「让会话常青」。
//! - **Cookie 的 `Path=/api`**：登录页是 SPA 静态资源（`/`、`/assets/*`），认证只需要
//!   覆盖 `/api/*`。收窄 Path 让静态资源请求完全不携带凭据。
//! - **登录密钥走 POST body**，不是 URL query —— 后者会进浏览器历史 / 反代访问日志。
//!
//! ## 不做什么
//!
//! - **网关不套 Cookie 登录**（`/v1/*`、57891/57892/57894/57895）：那里各用各的 Bearer key。
//!   但同一个 `BUDDY_SWITCH_AUTH_KEY` 也能直接当 Bearer 调 `/v1/*`（系统密钥，见
//!   [`buddy_switch_gateway::system_key`]）—— 不要求先建普通 API Key。
//! - **`GET /api/status` 免登录**：Docker / 负载均衡探针只需要存活信息。其余 `/api/*`
//!   仍受会话或系统密钥保护。
//! - **不保护静态资源**：`dist` 与 `index.html` 不含任何凭据，登录页本身也要能被加载。
//!   数据面全在 `/api/*` 上。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Extension;
use axum::{Json, Router};
use serde_json::{json, Value};

/// 管理面登录密钥的环境变量名（未设置 / 空串 = 不开启登录）。
pub const AUTH_KEY_ENV: &str = buddy_switch_gateway::system_key::SYSTEM_KEY_ENV;

/// 免登录的存活探针路径（与网关 `/health` 的分工：这里探的是管理面 + 进程）。
const PUBLIC_PATHS: [&str; 1] = ["/api/status"];

/// 会话 Cookie 的名字。
pub const SESSION_COOKIE: &str = "buddy_switch_session";

/// 会话有效期：7 天。服务重启后内存里的会话全部消失，需要重新登录。
pub const SESSION_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// 会话 Token 的长度（`Uuid::simple()` 的 32 个十六进制字符）。
const SESSION_TOKEN_LEN: usize = 32;

/// 运行时策略。启动时由 [`AuthPolicy::from_env`] 定一次，之后只读（`Clone` 进路由）。
///
/// **刻意不实现 `Debug` 的 derive**：见手写实现，任何 `{:?}` 都不该把密钥打进日志。
#[derive(Clone, PartialEq, Eq)]
pub struct AuthPolicy {
    key: Option<String>,
}

impl AuthPolicy {
    /// 从环境变量原始值构造。`None` / 空串 / 全空白 ⇒ 登录关闭。
    ///
    /// 取 env 值作**入参**而非直接读 `std::env`：测试可并行跑，不会互相踩环境变量
    /// （本仓既有约定，见 `main::resolve_host`）。
    pub fn from_env(raw: Option<&str>) -> Self {
        let key = raw
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Self { key }
    }

    /// 登录是否开启。
    pub fn enabled(&self) -> bool {
        self.key.is_some()
    }

    /// 候选密钥是否被接受。
    ///
    /// 比较**常量时间**（复用网关的实现，见 `buddy_switch_gateway::apikey::constant_time_eq`）：
    /// 这里比较的是管理员口令，逐字节短路的 `==` 在理论上可被计时侧信道逐位猜出。
    /// 候选值先 `trim` —— 用户从密码管理器粘贴时常带首尾空白。
    pub fn accepts(&self, candidate: &str) -> bool {
        let Some(key) = self.key.as_deref() else {
            return false;
        };
        buddy_switch_gateway::apikey::constant_time_eq(candidate.trim().as_bytes(), key.as_bytes())
    }
}

impl std::fmt::Debug for AuthPolicy {
    /// 只出「开 / 关」，**永不**出密钥：策略对象很容易被顺手 `{:?}` 进日志或报错信息，
    /// 而它唯一的秘密就是那个字符串。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPolicy")
            .field("enabled", &self.enabled())
            .finish()
    }
}

/// 会话表：`token -> 过期时刻`。**进程内**，重启即清空。
static SESSIONS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sessions() -> &'static Mutex<HashMap<String, Instant>> {
    &SESSIONS
}

/// 锁中毒不传染：会话表里没有「必须保持一致」的不变式，panic 过的锁照样能用
/// （与 `api.rs` 的既有写法一致）。
fn with_sessions<R>(f: impl FnOnce(&mut HashMap<String, Instant>) -> R) -> R {
    let mut guard = sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 新建一个会话，返回 Cookie 值。
pub fn new_session() -> String {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let expires_at = Instant::now() + Duration::from_secs(SESSION_TTL_SECS);
    with_sessions(|map| {
        // 顺手清掉过期条目：会话表只在登录时增长，不清就等于一个只增不减的泄漏点。
        let now = Instant::now();
        map.retain(|_, expires_at| *expires_at > now);
        map.insert(token.clone(), expires_at);
    });
    token
}

/// 会话是否仍然有效（过期即删除并返回 `false`）。
///
/// 先校验形状再查表：形状不对的串连哈希都不用算，也不会让表里出现垃圾键。
pub fn session_valid(token: &str) -> bool {
    if token.len() != SESSION_TOKEN_LEN
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    with_sessions(|map| match map.get(token) {
        Some(expires_at) if *expires_at > Instant::now() => true,
        Some(_) => {
            map.remove(token);
            false
        }
        None => false,
    })
}

/// 吊销一个会话（登出）。不存在的 token 静默忽略。
pub fn revoke_session(token: &str) {
    with_sessions(|map| {
        map.remove(token);
    });
}

/// 从 `Cookie` 头里取某个名字的值。
///
/// 只认**完整**名字（`session=...` 不会匹配 `my_session=...`）；值原样返回（不 trim、
/// 不去引号）—— 我们自己发的 Token 是纯十六进制，任何异常形状都会在
/// [`session_valid`] 那里被拒掉。
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then_some(value)
    })
}

/// 请求里的会话 Cookie 值。
pub fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie_value(raw, SESSION_COOKIE)
}

/// 请求是否来自 HTTPS（看反代的 `X-Forwarded-Proto`）。
///
/// 只有确认是 HTTPS 才加 `Secure`：直接跑 HTTP（SSH 隧道 / 局域网 / localhost）时
/// 加了 `Secure` 浏览器就**不会**回传这个 Cookie，用户会陷入「密码对了但一直回登录页」。
fn request_is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
}

/// 登录成功后下发的 `Set-Cookie`。
///
/// 无 `Domain`（只对当前主机生效）、`SameSite=Strict`（跨站请求根本不带这个 Cookie，
/// 于是 CSRF 在 Cookie 这一层就断了）、`HttpOnly`（JS 读不到，XSS 偷不走会话）。
fn set_cookie(token: &str, secure: bool) -> String {
    let mut value = format!(
        "{SESSION_COOKIE}={token}; Path=/api; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL_SECS}"
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// 登出时下发的 `Set-Cookie`（同名同属性，值清空 + 立即过期）。
fn clear_cookie(secure: bool) -> String {
    let mut value = format!("{SESSION_COOKIE}=; Path=/api; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// 给响应加 `Cache-Control: no-store`。
///
/// 认证响应能被中间缓存留下就是凭据泄漏（登录成功响应里还带着 `Set-Cookie`）。
fn with_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn auth_json(status: axum::http::StatusCode, body: Value) -> Response {
    with_no_store((status, Json(body)).into_response())
}

/// 给响应挂 `Set-Cookie`。
fn with_set_cookie(mut response: Response, cookie: String) -> Response {
    match HeaderValue::from_str(&cookie) {
        Ok(value) => {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        // 值全部由我们自己拼（Token 是十六进制），不可能失败；失败了也不该把
        // 已经成功的登录变成 500 —— 只是这一轮浏览器没拿到 Cookie。
        Err(_) => {}
    }
    response
}

/// 登录相关的三条公开路由（**不经** [`require_auth`]）。
pub fn routes() -> Router {
    Router::new()
        .route("/api/auth/status", get(api_auth_status))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
}

/// `GET /api/auth/status` —— 前端启动时问「要不要登录 / 现在登录了没」。
///
/// 登录关闭时回 `authenticated: true`：前端据此直接渲染应用，不必为「无认证」
/// 这条路径写第二套分支。
async fn api_auth_status(Extension(policy): Extension<AuthPolicy>, headers: HeaderMap) -> Response {
    if !policy.enabled() {
        return auth_json(
            axum::http::StatusCode::OK,
            json!({ "required": false, "authenticated": true }),
        );
    }
    let authenticated = cookie_token(&headers).is_some_and(session_valid);
    auth_json(
        axum::http::StatusCode::OK,
        json!({ "required": true, "authenticated": authenticated }),
    )
}

/// `POST /api/auth/login` —— 校验密钥，成功后下发会话 Cookie。
///
/// 401 响应**不带** `WWW-Authenticate`：带了浏览器会给这个域弹原生 Basic Auth 对话框，
/// 而我们的登录页在它后面，用户会看到两个要密码的框。
async fn api_auth_login(
    Extension(policy): Extension<AuthPolicy>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !policy.enabled() {
        // 关闭状态下这条端点没有意义，但回 200 而不是 404：老前端 / 手写脚本
        // 不该因为「登录没开」而看到一个报错。
        return auth_json(axum::http::StatusCode::OK, json!({ "ok": true }));
    }
    let candidate = body.get("key").and_then(Value::as_str).unwrap_or("");
    if !policy.accepts(candidate) {
        return auth_json(
            axum::http::StatusCode::UNAUTHORIZED,
            json!({ "ok": false, "error": "密钥不正确", "reason": "invalid_key" }),
        );
    }
    // 换发新 Token：旧会话（同一浏览器里可能还留着的那个）必须先失效，
    // 否则「重新登录」不会把可能已泄漏的旧 Cookie 收回。
    if let Some(previous) = cookie_token(&headers) {
        revoke_session(previous);
    }
    let token = new_session();
    let response = auth_json(axum::http::StatusCode::OK, json!({ "ok": true }));
    with_set_cookie(response, set_cookie(&token, request_is_https(&headers)))
}

/// `POST /api/auth/logout` —— 吊销当前会话并清 Cookie。
async fn api_auth_logout(headers: HeaderMap) -> Response {
    if let Some(token) = cookie_token(&headers) {
        revoke_session(token);
    }
    let response = auth_json(axum::http::StatusCode::OK, json!({ "ok": true }));
    with_set_cookie(response, clear_cookie(request_is_https(&headers)))
}

/// 从 `Authorization: Bearer <key>` 取候选系统密钥。
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// axum 中间件：把 [`AuthPolicy`] 套在 `api_routes()` 上。
///
/// 四条纪律：
/// 1. 登录关闭 ⇒ 直接放行（既有行为不变）；
/// 2. [`PUBLIC_PATHS`]（`GET /api/status`）⇒ 放行，供 Docker / 探针使用；
/// 3. Cookie 有效，或 `Authorization: Bearer <BUDDY_SWITCH_AUTH_KEY>` ⇒ 放行；
/// 4. 其余一律 401 JSON，**不带** `WWW-Authenticate`（同上，别惊动浏览器）。
///
/// 这里刻意**不**做 fail-open：拿不到 `State<AuthPolicy>` 是路由器装配错误，
/// axum 会直接 500，而不是「忘了装策略就放行所有人」。
pub async fn require_auth(
    State(policy): State<AuthPolicy>,
    request: Request,
    next: Next,
) -> Response {
    if !policy.enabled() || PUBLIC_PATHS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    if cookie_token(request.headers()).is_some_and(session_valid) {
        return next.run(request).await;
    }
    if bearer_token(request.headers()).is_some_and(|candidate| policy.accepts(candidate)) {
        return next.run(request).await;
    }
    auth_json(
        axum::http::StatusCode::UNAUTHORIZED,
        json!({ "ok": false, "error": "未登录或会话已过期", "reason": "unauthorized" }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::middleware::from_fn_with_state;
    use tower::ServiceExt;

    #[test]
    fn from_env_trims_and_treats_blank_as_disabled() {
        assert!(!AuthPolicy::from_env(None).enabled(), "None ⇒ 关闭");
        assert!(!AuthPolicy::from_env(Some("")).enabled(), "空串 ⇒ 关闭");
        assert!(
            !AuthPolicy::from_env(Some("   ")).enabled(),
            "全空白 ⇒ 关闭"
        );
        assert!(!AuthPolicy::from_env(Some("\t\n")).enabled());
        assert!(AuthPolicy::from_env(Some("k")).enabled(), "单字符也要生效");
        assert_eq!(
            AuthPolicy::from_env(Some("  secret  ")),
            AuthPolicy::from_env(Some("secret")),
            "首尾空白必须被吃掉"
        );
    }

    #[test]
    fn accepts_only_the_exact_key() {
        let policy = AuthPolicy::from_env(Some("test-key"));
        assert!(policy.accepts("test-key"));
        assert!(policy.accepts("  test-key\n"), "粘贴带空白也应接受");
        assert!(!policy.accepts("test-keY"), "大小写不同 ⇒ 拒");
        assert!(!policy.accepts("test-ke"), "短一位 ⇒ 拒");
        assert!(!policy.accepts("test-keyy"), "长一位 ⇒ 拒");
        assert!(!policy.accepts(""), "空候选 ⇒ 拒");
        assert!(
            !AuthPolicy::from_env(None).accepts(""),
            "关闭状态下不接受任何候选（不存在「空 key 万能」）"
        );
    }

    #[test]
    fn debug_never_leaks_the_key() {
        let policy = AuthPolicy::from_env(Some("super-secret-value"));
        let rendered = format!("{policy:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(rendered.contains("enabled: true"), "{rendered}");
        assert!(format!("{:?}", AuthPolicy::from_env(None)).contains("enabled: false"));
    }

    #[test]
    fn cookie_value_picks_the_exact_name_among_many() {
        let header = "theme=dark; buddy_switch_session=abc123; my_buddy_switch_session=nope";
        assert_eq!(
            cookie_value(header, SESSION_COOKIE),
            Some("abc123"),
            "必须按完整名字匹配，不能被相似名抢先"
        );
        assert_eq!(cookie_value(header, "theme"), Some("dark"));
        assert_eq!(cookie_value(header, "missing"), None);
        assert_eq!(cookie_value("", SESSION_COOKIE), None);
        assert_eq!(cookie_value("garbage", SESSION_COOKIE), None);
    }

    #[test]
    fn set_and_clear_cookie_carry_the_hardening_attributes() {
        let cookie = set_cookie("abc", false);
        assert!(cookie.starts_with("buddy_switch_session=abc; "), "{cookie}");
        for required in ["Path=/api", "HttpOnly", "SameSite=Strict", "Max-Age=604800"] {
            assert!(cookie.contains(required), "缺 {required}: {cookie}");
        }
        assert!(
            !cookie.contains("Secure"),
            "HTTP 直连时不能加 Secure: {cookie}"
        );
        assert!(!cookie.contains("Domain"), "不该限定 Domain: {cookie}");
        assert!(set_cookie("abc", true).contains("; Secure"));

        let cleared = clear_cookie(false);
        assert!(cleared.starts_with("buddy_switch_session=; "), "{cleared}");
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(cleared.contains("HttpOnly") && cleared.contains("SameSite=Strict"));
        assert!(clear_cookie(true).contains("; Secure"));
    }

    #[test]
    fn sessions_live_until_revoked() {
        let token = new_session();
        assert_eq!(token.len(), 32);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "token 必须是 32 位小写十六进制: {token}"
        );
        assert!(session_valid(&token));
        assert_ne!(token, new_session(), "每次登录都要换一个 Token");

        revoke_session(&token);
        assert!(!session_valid(&token), "登出后旧 Token 必须失效");
        // 重复吊销不是错误（登出按钮可能被点两次）。
        revoke_session(&token);
    }

    #[test]
    fn session_valid_rejects_foreign_shapes() {
        for bogus in [
            "",
            "abc",
            "0123456789abcdef0123456789abcde",   // 31
            "0123456789abcdef0123456789abcdef0", // 33
            "0123456789ABCDEF0123456789abcdef",  // 大写
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",  // 非十六进制
        ] {
            assert!(!session_valid(bogus), "不该认这个 Token: {bogus}");
        }
        // 形状合法但没登录过的 Token 也必须拒。
        assert!(!session_valid("0123456789abcdef0123456789abcdef"));
    }

    async fn probe(policy: AuthPolicy, cookie: Option<&str>) -> (StatusCode, String) {
        probe_with(policy, "/x", cookie, None).await
    }

    async fn probe_with(
        policy: AuthPolicy,
        uri: &str,
        cookie: Option<&str>,
        authorization: Option<&str>,
    ) -> (StatusCode, String) {
        let app = Router::new()
            .route("/x", get(|| async { "ok" }))
            .route("/api/status", get(|| async { "ok" }))
            .layer(from_fn_with_state(policy, require_auth));
        let mut builder = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if let Some(authorization) = authorization {
            builder = builder.header(header::AUTHORIZATION, authorization);
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .expect("router call");
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|value| value.to_str().ok()),
                Some("no-store"),
                "认证失败响应必须 no-store"
            );
            assert!(
                response.headers().get(header::WWW_AUTHENTICATE).is_none(),
                "不能带 WWW-Authenticate（否则浏览器会弹原生登录框）"
            );
        }
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn middleware_blocks_without_session_and_allows_with_one() {
        let policy = AuthPolicy::from_env(Some("test-key"));

        let (status, body) = probe(policy.clone(), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("unauthorized"), "{body}");

        let (status, _) = probe(policy.clone(), Some("buddy_switch_session=abc")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "形状不对的 Token 也要拒");

        let token = new_session();
        let (status, body) = probe(
            policy.clone(),
            Some(&format!("buddy_switch_session={token}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "有效会话必须放行: {body}");

        // 关闭状态下连乱写的 Cookie 也不影响。
        let (status, _) = probe(AuthPolicy::from_env(None), Some("x=y")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn status_probe_is_public_even_when_login_is_enabled() {
        let policy = AuthPolicy::from_env(Some("test-key"));
        let (status, body) = probe_with(policy.clone(), "/api/status", None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // 其余 `/api/*` 仍受保护。
        let (status, _) = probe(policy, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_system_key_unlocks_management_api() {
        let policy = AuthPolicy::from_env(Some("test-key"));

        let (status, body) = probe_with(policy.clone(), "/x", None, Some("Bearer test-key")).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // 大小写不敏感的前缀、首尾空白都接受。
        let (status, _) = probe_with(policy.clone(), "/x", None, Some("bearer  test-key ")).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = probe_with(policy.clone(), "/x", None, Some("Bearer wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // 没有 Bearer 前缀的裸值不当成密钥。
        let (status, _) = probe_with(policy, "/x", None, Some("test-key")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
