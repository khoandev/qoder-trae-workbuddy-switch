# Auth bằng env key cho webui

## Context

Webui quản trị của `buddy-switch-server` hiện **không có đăng nhập**; ai vào được cổng 57890 là đọc/ghi được toàn bộ账号库 và gateway key (xem `docs/guides/DOCKER.md` §安全). Yêu cầu: thêm lớp đăng nhập bằng **một key lấy từ biến môi trường**, dùng **trang đăng nhập riêng + cookie phiên HttpOnly** (người dùng đã chọn), không dùng Basic Auth.

Phạm vi: chỉ **management surface** của `buddy-switch-server` (`buddy-switch serve`, npm CLI, Docker). Tauri desktop dùng `invoke`, không có HTTP management surface nên không bị ảnh hưởng. Các cổng API gateway (`/v1/*`, 57891/57892/57894/57895) giữ nguyên Bearer key riêng; **không** áp dụng key này cho gateway.

Trạng thái cuối:
- `BUDDY_SWITCH_AUTH_KEY` chưa set/rỗng → auth tắt, hành vi y hệt hiện tại.
- Set key → mở webui thấy trang đăng nhập; nhập đúng key → server đặt cookie phiên; `/api/*` không có cookie hợp lệ trả `401`.
- Có nút đăng xuất trong Settings; session sống 7 ngày, lưu **in-memory** (restart server → đăng nhập lại).

## Approach

### 1. Server: module `app_auth` (mới)

Tạo `crates/buddy-switch-server/src/app_auth.rs`.

Hằng số / policy:

```rust
pub const AUTH_KEY_ENV: &str = "BUDDY_SWITCH_AUTH_KEY";
pub const SESSION_COOKIE: &str = "buddy_switch_session";
pub const SESSION_TTL_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, PartialEq, Eq)]
pub struct AuthPolicy { key: Option<String> }

impl AuthPolicy {
    /// trim; None/rỗng/toàn khoảng trắng => auth tắt.
    pub fn from_env(raw: Option<&str>) -> Self;
    pub fn enabled(&self) -> bool;
    /// so sánh constant-time trên `candidate.trim().as_bytes()`.
    pub fn accepts(&self, candidate: &str) -> bool;
}
```

- `accepts` dùng `buddy_switch_gateway::apikey::constant_time_eq` (xem bước 2). Không tự viết hàm so sánh thứ hai.
- Viết `impl Debug for AuthPolicy` thủ công, chỉ in `enabled`, **không in key** (tránh lộ key khi ai đó `{:?}` policy).
- Không thêm minimum length; key rỗng = tắt. Tài liệu khuyến nghị key dài ≥16 ký tự.

Session store:

```rust
static SESSIONS: OnceLock<Mutex<HashMap<String, Instant>>>;
```

- Token: `uuid::Uuid::new_v4().simple().to_string()` (32 ký tự hex thường).
- `SESSION_TTL_SECS` = 604800.
- `new_session() -> String`: prune entry hết hạn, chèn token với `Instant::now() + Duration::from_secs(SESSION_TTL_SECS)`.
- `session_valid(token) -> bool`: chỉ nhận token dài đúng 32 và toàn ASCII hex; hết hạn thì xoá + `false`.
- `revoke_session(token)`: remove khỏi map.
- `Mutex` lock dùng `.unwrap_or_else(|poisoned| poisoned.into_inner())` theo style hiện có.

Cookie helpers:

- `cookie_value(header: &str, name: &str) -> Option<&str>`: parse `Cookie` bằng `split(';')` + `split_once('=')`; tên so khớp chính xác.
- `cookie_token(headers: &HeaderMap) -> Option<&str>`: lấy header `cookie` rồi gọi `cookie_value`.
- `request_is_https(headers)`: header `x-forwarded-proto`, lấy phần trước dấu phẩy đầu tiên, trim, so `eq_ignore_ascii_case("https")`.
- `set_cookie(token, secure)` trả đúng chuỗi:

```text
buddy_switch_session=<token>; Path=/api; HttpOnly; SameSite=Strict; Max-Age=604800
```

  thêm `; Secure` khi `secure == true`. Không dùng `Domain`.
- `clear_cookie(secure)` cùng attribute nhưng value rỗng và `Max-Age=0`.

Routes / handlers:

```rust
pub fn routes() -> Router {
    Router::new()
        .route("/api/auth/status", get(api_auth_status))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
}
```

Handler lấy policy qua `axum::Extension<AuthPolicy>` (bắt buộc — thiếu extension là 500, **không fail-open**).

| Endpoint | Request | Response |
|:--|:--|:--|
| `GET /api/auth/status` | không cần auth | `200 {"required": bool, "authenticated": bool}`. Auth tắt ⇒ `required:false, authenticated:true`. Auth bật ⇒ `authenticated` = cookie hợp lệ. |
| `POST /api/auth/login` | JSON `{"key":"<string>"}` | Auth tắt ⇒ `200 {"ok":true}` (không set cookie). Sai key ⇒ `401 {"ok":false,"error":"密钥不正确","reason":"invalid_key"}`. Đúng ⇒ revoke session cũ (nếu có), tạo session mới, `200 {"ok":true}` + `Set-Cookie`. |
| `POST /api/auth/logout` | body không bắt buộc | Revoke token trong cookie (nếu có), `200 {"ok":true}` + `Set-Cookie` xoá cookie. |

- Login dùng `Json<Value>`; key lấy `body.get("key").and_then(Value::as_str).unwrap_or("")`.
- Mọi response auth thêm header `Cache-Control: no-store`.
- Response `401` **không** kèm `WWW-Authenticate` (tránh browser bật Basic Auth prompt).
- Middleware:

```rust
pub async fn require_auth(
    State(policy): State<AuthPolicy>,
    request: Request,
    next: Next,
) -> Response
```

  `!policy.enabled()` ⇒ cho qua. Có cookie hợp lệ ⇒ cho qua. Ngược lại:

```text
401 {"ok":false,"error":"未登录或会话已过期","reason":"unauthorized"}
```

  Middleware chỉ được gắn lên `api_routes()`, không gắn lên `app_auth::routes()`, static assets, hay gateway.

Unit tests trong `app_auth.rs`:
- `from_env` trim/blank; `accepts` đúng key, sai 1 byte, sai độ dài, rỗng.
- `format!("{policy:?}")` không chứa key.
- `cookie_value` tìm đúng cookie giữa nhiều cookie.
- `set_cookie`/`clear_cookie` chứa `HttpOnly`, `SameSite=Strict`, `Path=/api`, `Max-Age=604800`; biến `secure` thêm `Secure`.
- `new_session` → `session_valid` true; `revoke_session` → false; token sai shape → false.
- Middleware: router nhỏ `Router::new().route("/x", get(...)).layer(from_fn_with_state(policy, require_auth))`; không cookie ⇒ 401, có cookie session ⇒ 200, auth tắt ⇒ 200.

### 2. Gateway: public hoá `constant_time_eq`

`crates/buddy-switch-gateway/src/apikey.rs` dòng ~847: đổi `pub(crate) fn constant_time_eq` thành `pub fn constant_time_eq`. Cập nhật doc comment: dùng chung cho Trae/Qoder multi-key **và** `buddy-switch-server` management login; **không** copy hàm này sang server.

### 3. Wire router + startup

`crates/buddy-switch-server/Cargo.toml`: thêm dependency `uuid = { version = "1", features = ["v4"] }`. `uuid` đã có trong `Cargo.lock` (core dùng), không phát sinh package mới.

`crates/buddy-switch-server/src/main.rs`:
- Thêm `mod app_auth;` cạnh `mod api;`.
- Trong `serve()`, thay `let app = api::router();` bằng:

```rust
let auth_key = std::env::var(app_auth::AUTH_KEY_ENV).ok();
let auth_policy = app_auth::AuthPolicy::from_env(auth_key.as_deref());
let app = api::router_with_auth(auth_policy.clone());
```

- Sau dòng `println!("按 Ctrl+C 停止服务。");` (hoặc ngay sau banner webui):
  - `auth_policy.enabled()` ⇒ `println!("管理面登录: 已开启（密钥来自 {}）", app_auth::AUTH_KEY_ENV);`
  - env có mặt nhưng trim rỗng ⇒ `println!("⚠️  {} 为空，管理面登录未开启。", app_auth::AUTH_KEY_ENV);`
  - **Không in key**.
- Sửa cảnh báo non-loopback hiện có: thêm lựa chọn `BUDDY_SWITCH_AUTH_KEY`:
  `"... 或用反向代理加上鉴权，或设置 BUDDY_SWITCH_AUTH_KEY 开启内置登录。"`
- Sau `runtime_log::init(...)`: nếu auth bật, `runtime_log::info("startup", "管理面登录已开启");` (không log key).

`crates/buddy-switch-server/src/api.rs`:
- Đổi `pub fn router()` thành `pub(crate) fn router_with_auth(policy: app_auth::AuthPolicy) -> Router`.
- Nội dung:

```rust
let gateway_state = crate::gateway_host::shared_state();

let management = app_auth::routes()
    .merge(
        api_routes().layer(axum::middleware::from_fn_with_state(
            policy.clone(),
            app_auth::require_auth,
        )),
    )
    .layer(axum::Extension(policy))
    .layer(axum::middleware::from_fn(crate::api_guard::guard));

management
    .merge(buddy_switch_gateway::router(gateway_state))
    .fallback(static_handler)
```

- Giữ nguyên thứ tự: `origin guard` ngoài cùng cho toàn bộ management routes; auth middleware chỉ trên `api_routes()`; gateway merge + static fallback sau cùng.
- Cập nhật doc comment: static assets (`dist`, index.html) **cố ý public** — chúng không chứa credential và login page do SPA render; auth chỉ chặn `/api/*`.
- Cập nhật 3 helper test đang gọi `router()` (`call_full`, `status_with_headers`, `call_cache_control`) thành helper mới:

```rust
fn test_router() -> Router {
    router_with_auth(crate::app_auth::AuthPolicy::from_env(None))
}
```

  nhờ đó toàn bộ test cũ chạy với auth tắt như trước.

Thêm integration test trong `api.rs::tests`:

`auth_login_unlocks_management_api_and_logout_revokes_session`
- `router_with_auth(AuthPolicy::from_env(Some("test-key")))`.
- `GET /` (Host `127.0.0.1:57890`) ⇒ `200`, content-type `text/html` (static public).
- `GET /api/status` không cookie ⇒ `401`; assert **không** có `www-authenticate`.
- `GET /api/auth/status` không cookie ⇒ `200`, `required:true`, `authenticated:false`.
- `POST /api/auth/login` JSON `{"key":"wrong"}` ⇒ `401` + `reason:"invalid_key"`.
- `POST /api/auth/login` JSON `{"key":"test-key"}` ⇒ `200`, `Set-Cookie` có `buddy_switch_session=`.
- Lấy cặp `name=value` đầu tiên của `Set-Cookie`; gửi lại trong header `cookie`.
- `GET /api/status` với cookie ⇒ `200`.
- `GET /api/auth/status` với cookie ⇒ `required:true`, `authenticated:true`.
- `POST /api/auth/logout` với cookie ⇒ `200`, `Set-Cookie` có `Max-Age=0`.
- `GET /api/status` với cookie cũ ⇒ `401`.

Thêm test `auth_disabled_keeps_management_api_open`: policy `from_env(None)`; `GET /api/status` có Host ⇒ `200`; `/api/auth/status` ⇒ `{required:false, authenticated:true}`.

### 4. Frontend: auth store + xử lý 401

`src/lib/api.ts`:
- Cạnh `API_BASE` (dòng ~170) export:

```ts
export const AUTH_REQUIRED_EVENT = "buddy-switch:auth-required";
export function apiBaseUrl(): string { return API_BASE; }
```

- Trong `httpCall` (dòng ~789), ngay sau khi có `res` và trước `res.json()`:

```ts
if (res.status === 401) {
  if (typeof window !== "undefined") {
    window.dispatchEvent(new Event(AUTH_REQUIRED_EVENT));
  }
  throw new Error(t("auth.session.expired"));
}
```

  Không đổi `credentials` của `fetch` (mặc định `same-origin` đã gửi cookie cùng origin).

Tạo `src/lib/webui-auth.ts`:
- Import `useSyncExternalStore`, `apiBaseUrl`, `AUTH_REQUIRED_EVENT`, `isWebui`, `isDemoMode`, `t`.
- `export type WebuiAuthStatus = "unknown" | "disabled" | "required" | "authenticated";`
- Initial status: `!isWebui() || isDemoMode() ? "disabled" : "unknown"`.
- Module-level `status`, `listeners`, `setStatus`; `useWebuiAuth()` dùng `useSyncExternalStore`.
- `refreshWebuiAuth()`:
  - Nếu `disabled` ⇒ return.
  - `fetch(`${apiBaseUrl()}/api/auth/status`, { credentials: "same-origin", cache: "no-store" })`.
  - Thành công: `required === false` ⇒ `disabled`; `required && authenticated` ⇒ `authenticated`; còn lại ⇒ `required`.
  - Lỗi mạng/parse ⇒ `required` (để login page hiện và báo lỗi khi submit).
- `loginWebui(key)`:
  - `POST ${apiBaseUrl()}/api/auth/login`, header `Content-Type: application/json`, `credentials: "same-origin"`, body `JSON.stringify({ key })`.
  - Lỗi mạng ⇒ throw `t("shared.api.unreachable", { base: apiBaseUrl() })`.
  - `401` ⇒ throw `t("auth.login.invalid")`; lỗi khác ⇒ `data.error || t("auth.login.failed")`.
  - Thành công ⇒ `setStatus("authenticated")`.
- `logoutWebui()`:
  - `POST ${apiBaseUrl()}/api/auth/logout`, header JSON, body `"{}"`, `credentials: "same-origin"`.
  - Không OK ⇒ throw `t("auth.logout.failed")`; OK ⇒ `setStatus("required")`.
- Đăng ký listener một lần khi module load, chỉ khi `isWebui() && !isDemoMode()`:

```ts
window.addEventListener(AUTH_REQUIRED_EVENT, () => setStatus("required"));
```

### 5. Frontend: login page, gate, logout, i18n

Tạo `src/components/auth-gate.tsx`:

```tsx
export function AuthGate({ children }: { children: ReactNode }) {
  const status = useWebuiAuth();
  useEffect(() => { void refreshWebuiAuth(); }, []);
  if (status === "unknown") { /* full-screen Loader2, aria-busy */ }
  if (status === "required") return <LoginPage />;
  return <>{children}</>;
}
```

Tạo `src/components/login-page.tsx`:
- Full-screen centered `Card` (dùng `Card/CardHeader/CardTitle/CardDescription/CardContent`, `Label`, `Input`, `Button`, `Alert` — không tự dựng form primitive).
- Title `t("auth.login.title")`, subtitle `t("auth.login.subtitle")`.
- Input `type="password"`, `autoFocus`, `autoComplete="current-password"`, `spellCheck={false}`; label/key placeholder theo i18n.
- Submit disable khi key rỗng hoặc đang gửi; gọi `loginWebui(key.trim())`; lỗi hiển thị trong `Alert variant="destructive"` bằng `asError(e)`.
- Hint dưới form: `t("auth.login.hint")` (nói key lấy từ `BUDDY_SWITCH_AUTH_KEY`).

`src/main.tsx`: bọc cả hai nhánh trong `AuthGate`:

```tsx
<ErrorBoundary>
  <AuthGate>
    {isCompanionWindow ? <CompanionApp /> : <App />}
  </AuthGate>
</ErrorBoundary>
```

`src/components/app-settings.tsx`:
- Import `LogOut`, `useWebuiAuth`, `logoutWebui`.
- Trong `AppSettingsGroups` (~dòng 870): `const auth = useWebuiAuth();` và render `SessionCard` khi `api.isWebui() && auth === "authenticated"` (auth tắt/desktop/demo ⇒ không hiện).
- Thêm `SessionCard`:
  - `SettingsGroup id="settings-session" title={t("auth.session.title")} variant={variant}` + `GroupBody`.
  - `SettingsFieldRow stacked={variant === "plain"} label={t("auth.logout")} description={t("auth.logout.description")}` + `Button variant="outline" size="sm"`.
  - Click: `setBusy(true); try { await logoutWebui(); } catch (e) { toast.error(asError(e)); setBusy(false); }`. Thành công thì AuthGate tự chuyển sang login (component unmount, không setState sau unmount).

i18n: thêm đúng các key sau vào `src/locales/domains/shell.zh.ts` và `src/locales/domains/shell.en.ts`:
- `auth.login.title`, `auth.login.subtitle`, `auth.login.keyLabel`, `auth.login.keyPlaceholder`, `auth.login.submit`, `auth.login.submitting`, `auth.login.invalid`, `auth.login.failed`, `auth.login.hint`
- `auth.session.title`, `auth.logout`, `auth.logout.description`, `auth.logout.failed`, `auth.session.expired`

Nội dung zh (en viết tương ứng):
- `auth.login.title`: `登录 Buddy Switch`
- `auth.login.subtitle`: `服务已开启访问保护。请输入服务端配置的访问密钥。`
- `auth.login.keyLabel`: `访问密钥`
- `auth.login.keyPlaceholder`: `BUDDY_SWITCH_AUTH_KEY`
- `auth.login.submit`: `登录`
- `auth.login.submitting`: `正在验证…`
- `auth.login.invalid`: `密钥不正确。`
- `auth.login.failed`: `登录失败，请稍后重试。`
- `auth.login.hint`: `密钥由服务端环境变量 BUDDY_SWITCH_AUTH_KEY 提供；登录凭据保存在 HttpOnly Cookie 中。`
- `auth.session.title`: `登录与安全`
- `auth.logout`: `退出登录`
- `auth.logout.description`: `让服务端吊销当前会话，并清除浏览器中的登录 Cookie。`
- `auth.logout.failed`: `退出登录失败，请检查服务是否在线。`
- `auth.session.expired`: `登录已过期，请重新登录。`

Không dùng `localStorage` cho key; chỉ cookie HttpOnly do server set.

### 6. Docs: bỏ các câu đã sai

- `docs/guides/DOCKER.md`:
  - Đổi tiêu đề `## 配置：三个环境变量` → `## 配置：环境变量`.
  - Thêm row vào bảng env:
    `| BUDDY_SWITCH_AUTH_KEY | 空（不开登录） | 管理面登录密钥。设置后访问 webui 需先输入该密钥；会话 Cookie 有效期 7 天，服务重启后需重新登录。建议用 ≥16 位随机串。 |`
  - §安全: câu mở đầu đổi thành: `**webui 默认没有登录；设置 BUDDY_SWITCH_AUTH_KEY 后，访问管理面需先输入该密钥（HttpOnly + SameSite=Strict 会话 Cookie，7 天有效，服务重启后失效）。**` Thêm ghi chú key đi trong POST body nên phải dùng HTTPS/TLS khi mở ra ngoài.
- `docker-compose.yml`:
  - Comment cảnh báo dòng 18: `...本身**没有登录**` → `...默认没有登录；设置 BUDDY_SWITCH_AUTH_KEY 可开启登录`.
  - Trong `environment:` thêm dòng comment (không set mặc định):
    `# BUDDY_SWITCH_AUTH_KEY: "change-me-to-a-long-random-string"   # 不设 = 不开登录`
- `docs/guides/LINUX-WEBUI-FALLBACK.md` dòng ~76: thêm `BUDDY_SWITCH_AUTH_KEY` như cách bật đăng nhập trước khi khuyến nghị chỉ bind loopback.
- `docs/specs/KEY-PERMISSION-UI-DESIGN.md` §10.4 (~dòng 523): thay câu `api_guard ... **不做身份认证**` bằng: `api_guard 只做 Host / Origin 校验；设置 BUDDY_SWITCH_AUTH_KEY 后由 app_auth 的会话 Cookie 做身份认证。` Đồng thời sửa câu "局域网内任何设备都能直接拉走明文" thành "若未设置 BUDDY_SWITCH_AUTH_KEY, ...".

Không thêm release notes/changelog.

## Critical files & anchors

| File | Anchor | Why |
|:--|:--|:--|
| `crates/buddy-switch-server/src/app_auth.rs` | file mới | Toàn bộ policy/session/cookie/middleware/routes. |
| `crates/buddy-switch-server/src/api.rs` | `pub fn router()` dòng ~90 | Thay composition: auth routes + protected api + origin guard + gateway/fallback. |
| `crates/buddy-switch-server/src/main.rs` | `async fn serve` dòng ~502-555 | Đọc env, truyền policy, log trạng thái, sửa cảnh báo non-loopback. |
| `src/lib/api.ts` | `API_BASE` dòng ~170; `httpCall` dòng ~789 | Export base/event; 401 → login. |
| `src/main.tsx` + `src/components/app-settings.tsx` | render ~24; `AppSettingsGroups` ~870 | Gate toàn app; nút logout. |

## Verification

1. **Rust tests**
   ```bash
   cargo test -p buddy-switch-server
   ```
   Expected: pass toàn bộ; có 4 nhóm test mới (policy/cookie/session/middleware + 2 integration).
   Mutation check: bỏ `.layer(from_fn_with_state(..., app_auth::require_auth))` ⇒ integration test `auth_login_unlocks...` phải đỏ (401 test không còn 401).

2. **Frontend type + gate**
   ```bash
   npm run check:api
   npx tsc -p tsconfig.json --noEmit
   npm run build
   ```
   Expected: `check:api` pass (không thêm ROUTES entry; auth dùng fetch trực tiếp), `tsc` không lỗi, `npm run build` xanh (gồm `check:docker` sau khi sửa docs).

3. **Manual E2E backend**
   ```bash
   npm run build:fast
   BUDDY_SWITCH_HOME="$(mktemp -d)" BUDDY_SWITCH_AUTH_KEY=test-key-123456 \
     cargo run -p buddy-switch-server -- serve --no-open --port 57899
   ```
   Ở terminal khác:
   - `curl -i http://127.0.0.1:57899/api/status` ⇒ `401`, không có `WWW-Authenticate`.
   - `curl -i -H 'Content-Type: application/json' -d '{"key":"wrong"}' http://127.0.0.1:57899/api/auth/login` ⇒ `401`, reason `invalid_key`.
   - `curl -i -c /tmp/bs-cookie -H 'Content-Type: application/json' -d '{"key":"test-key-123456"}' http://127.0.0.1:57899/api/auth/login` ⇒ `200` + `Set-Cookie: buddy_switch_session=...; Path=/api; HttpOnly; SameSite=Strict; Max-Age=604800`.
   - `curl -i -b /tmp/bs-cookie http://127.0.0.1:57899/api/status` ⇒ `200`.
   - `curl -i -b /tmp/bs-cookie -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:57899/api/auth/logout` ⇒ `200` + `Max-Age=0`.
   - Gọi lại `/api/status` với cookie cũ ⇒ `401`.
   - `curl -i http://127.0.0.1:57899/` ⇒ `200 text/html` (login page load được).

4. **Manual UI** (cùng server trên, mở browser):
   - `http://127.0.0.1:57899/` ⇒ trang đăng nhập, không lộ dữ liệu.
   - Nhập sai ⇒ alert `密钥不正确` (hoặc bản en), không vào app.
   - Nhập đúng ⇒ app render, gọi API bình thường.
   - Settings → 登录与安全 → 退出登录 ⇒ quay lại trang đăng nhập; refresh vẫn ở trang đăng nhập.
   - Restart server rồi refresh ⇒ yêu cầu đăng nhập lại (session in-memory).
   - Set `BUDDY_SWITCH_AUTH_KEY` rỗng (`BUDDY_SWITCH_AUTH_KEY=""`) ⇒ mở webui vào thẳng app (auth tắt), log có cảnh báo "为空".

## Assumptions & contingencies

- Tên biến môi trường là `BUDDY_SWITCH_AUTH_KEY`; đổi tên chỉ cần sửa `app_auth::AUTH_KEY_ENV`.
- Session **in-memory**, TTL 7 ngày; restart server làm mất session. Nếu cần sống qua restart, thay bằng cookie ký HMAC — ngoài phạm vi hiện tại, không tự ý đổi.
- Static assets (`/`, `/assets/*`, SPA fallback) vẫn public; bảo vệ dữ liệu nằm ở `/api/*`. Login page do React render, không cần một route HTML riêng.
- Gateway `/v1/*` không chịu key này (giữ Bearer key riêng). Nếu gắn auth middleware lên gateway, client OpenAI/Anthropic hiện có sẽ hỏng.
- `Secure` chỉ bật khi thấy `X-Forwarded-Proto: https`; chạy HTTP trực tiếp (SSH tunnel/localhost) vẫn đăng nhập được. Khi mở ra Internet phải dùng TLS/reverse proxy.
- Reverse proxy rewrite `Host` về `127.0.0.1:57890` có thể làm login POST bị `403 cross_origin` do `Origin` của browser vẫn là domain ngoài. Cách đúng: giữ nguyên `Host` và thêm domain vào `BUDDY_SWITCH_ALLOWED_HOSTS` (đã có trong DOCKER.md), hoặc rewrite cả `Origin` ở proxy. Không nới `api_guard` trong thay đổi này.
- Nếu `uuid` không dùng được vì lý do feature/lock, fallback: `getrandom = "0.2"` (đã có trong lock) + tự encode 32 byte thành hex; giữ nguyên token shape 32 hex.
- `npm run dev` thuần browser là cross-origin (Vite 1420 → API 57890) nên cookie auth không hoạt động ở chế độ đó; kiểm thử auth bằng bản `dist` do server phục vụ (như mục Verification). Không thêm CORS chỉ để phục vụ dev.
