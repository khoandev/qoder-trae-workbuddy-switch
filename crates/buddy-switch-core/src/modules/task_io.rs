//! 任务引擎的**唯一出网点**（seam）。
//!
//! 设计依据：`docs/specs/WB-AUTOMATION-ARCH.md` §3.1 / §3.2。三句话说完：
//!
//! 1. **任务模块不许直接调 HTTP**：内核是纯函数（`plan()`），所有网络都在这个 trait 后面，
//!    这样 24 条成长任务 + 小程序 + 运营活动 + 盲盒的判据可以**离线**测。
//! 2. **每次请求都过风控准入**：[`RealTaskIo`] 在真正发出前调
//!    [`crate::modules::risk_control::admit_platform_request`]，被拦就**不发**（返回 `status = 0`
//!    + `code = -3`，见 [`ADMISSION_BLOCKED_CODE`]），响应回来后再 `record_platform_response`。
//!    这一层是「账号暂停 / 冷却 / 限速」的唯一落点，少调一次就等于风控设置对任务链失效。
//! 3. **凭据不进日志**：[`TaskCall`] 刻意不实现 `Debug`（它持有账号 JSON），日志要用
//!    [`call_label`]（URL 先过 `runtime_log::redact`）。
//! 4. **出口熔断按出口计**（opt-in `automation.egress_backoff`）：准入之前先问
//!    `egress::BREAKER`，`Defer` / `Waiting` 就直接返回 `status = 0` + [`EGRESS_DEFERRED_CODE`]；
//!    请求回来后按 [`failure_class`] 记账 —— 业务 4xx **不**算出口失败（spec §7.2）。
//!
//! 与既有模块的关系：`cat.rs` / `travel.rs` / `activity.rs` 各自的 `*Io` trait 是**同一形态的
//! 前身**（如 `CatIo`）。本文件不替换它们 —— 迁移期两套并存，等任务链全部搬完再收口
//! （见 `WB-AUTOMATION-ARCH.md` §10）。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::modules::backoff::{Admission, FailureClass};
use crate::modules::config::{authed_json_request_for, now_ms};
use crate::modules::device_identity;
use crate::modules::egress;
use crate::modules::region::Region;
use crate::modules::risk_control::{admit_platform_request, record_platform_response};
use crate::modules::runtime_log::redact;

/// 风控准入拦截的**哨兵码**：`status = 0`（没发出去）+ `code = -3`。
///
/// 与 `travel.rs` 的既有写法对齐（那边用 `code = -3` + 「风控准入拦截：…」文案），
/// 调用方据此区分「上游说不行」和「本地没让发」。
pub const ADMISSION_BLOCKED_CODE: i64 = -3;

/// 出口熔断挡住本次请求的**哨兵码**：`status = 0`（没发出去）+ `code = -4`。
///
/// 与 [`ADMISSION_BLOCKED_CODE`] 分开：那是「账号这一轮别动」，这是「这条出口还在冷却」。
/// 调用方据此决定是停账号本回合，还是只推迟这一条 feature。
pub const EGRESS_DEFERRED_CODE: i64 = -4;

/// 任务链的一次失败该怎么记到**出口**上（`None` = 这不是出口的锅，别记）。
///
/// 只有 407 计入熔断：任务链上的 400 / 401 / 403 / business `code != 0` 全是**正常业务态**
/// （`400 task not completed`、`403 连登天数不足`、token 过期）—— 把它们记到出口头上，
/// 三次「还没到」就会把整条出口熔断 30 分钟（spec §7.2）。
pub fn failure_class(status: u16) -> Option<FailureClass> {
    match status {
        // 请求没发出去（连接失败 / 超时）：这一条出口此刻有问题，但只是「这一次」。
        0 => Some(FailureClass::Transient),
        // 代理认证失败：出口本身配错了，再发也是同一个结果。
        407 => Some(FailureClass::Hard),
        429 => Some(FailureClass::RateLimit),
        500..=599 => Some(FailureClass::Transient),
        _ => None,
    }
}

/// 一次任务的 HTTP 结果：`(HTTP 状态码, 解析后的 JSON)`。
///
/// `status = 0` 表示**请求没有发出**（被风控准入拦下 / 凭据缺失在本地短路）。
pub type TaskReply = (u16, Value);

/// 一次任务请求的描述。
///
/// ⚠️ **不实现 `Debug` / `Display`**：`account` 里带 access token，任何「顺手打印一下请求」
/// 的代码都会把它带进日志。要打印就用 [`call_label`]。
pub struct TaskCall<'a> {
    pub region: Region,
    pub url: &'a str,
    /// 账号 JSON（`uid` / token / 程序位都在里面，与 `authed_json_request_for` 同一形状）。
    pub account: &'a Value,
    /// 追加请求头（`x-client-platform`、小程序标头、活动域 `Origin`/`Referer` 等）。
    pub headers: HashMap<String, String>,
}

impl<'a> TaskCall<'a> {
    pub fn new(region: Region, url: &'a str, account: &'a Value) -> Self {
        Self {
            region,
            url,
            account,
            headers: HashMap::new(),
        }
    }

    /// 追加一个请求头（链式，便于各 feature 描述自己的「像哪个客户端」）。
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// 追加一组请求头（小程序 / 活动域那种成套标头）。
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers.extend(headers);
        self
    }
}

/// 任务引擎的出网接口。实现必须 `Send + Sync`（调度器在 `tokio::spawn` 里调用）。
///
/// 这一层**只管发**：任务语义、间隔、幂等都不在 trait 里（间隔由内核的 `gap` + 台账负责，
/// 风控预算由 `admit_platform_request` 负责）。做成 trait 的唯一目的，是让离线测试
/// 能驱动真实入口而不发真实请求。
pub trait TaskIo: Send + Sync {
    /// 发起一次请求。`method` 只允许 `"GET"` / `"POST"`（任务链没有别的方法）。
    fn request<'a>(
        &'a self,
        call: TaskCall<'a>,
        method: &'a str,
        body: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = TaskReply> + Send + 'a>>;

    /// 只读查询（任务清单、盲盒额度、活动状态）。**同样过风控准入**。
    fn get<'a>(
        &'a self,
        call: TaskCall<'a>,
    ) -> Pin<Box<dyn Future<Output = TaskReply> + Send + 'a>> {
        self.request(call, "GET", None)
    }

    /// 写请求（接取 / 上报 / 领奖 / 开盲盒）。
    fn post<'a>(
        &'a self,
        call: TaskCall<'a>,
        body: Value,
    ) -> Pin<Box<dyn Future<Output = TaskReply> + Send + 'a>> {
        self.request(call, "POST", Some(body))
    }
}

/// WorkBuddy 的产品名（`risk_control` 的作用域键）。
pub const WORKBUDDY_PRODUCT: &str = "workbuddy";

/// 任务链的**唯一真实请求出口**：准入 → 发送 → 结果回写。
///
/// `activity.rs` / `cat.rs` 以及后续的 `wb_tasks` 都必须走这里，不许自己拼
/// `authed_json_request_for`：那些路径会**静默绕过** RPM / 并发 / 最小间隔 / 账号冷却
/// （`docs/specs/WB-AUTOMATION-ARCH.md` §3.1）。源码级闸门见测试
/// `task_modules_never_call_http_directly`。
///
/// 返回 `(0, {"code": -3})` 表示**本地没让发**（风控准入拦截），不是上游失败。
pub async fn send(
    product: &str,
    region: Region,
    url: &str,
    method: &str,
    body: Option<Value>,
    account: &Value,
    extra_headers: &HashMap<String, String>,
) -> TaskReply {
    let region_code = region.as_str();
    let uid = account.get("uid").and_then(Value::as_str).unwrap_or("");
    // 出口熔断：**opt-in**（`automation.egress_backoff`）。关着时不读槽表、不碰熔断表，
    // 这条路径与改造前逐字一致。
    let egress_key = if egress::backoff_enabled(account) {
        let resolution = egress::resolve_account(account);
        match egress::BREAKER.admit(resolution.key(), now_ms().max(0) as u64) {
            Admission::Allow | Admission::Probe => Some(resolution.key().clone()),
            Admission::Defer { until_ms } => {
                return (
                    0,
                    json!({
                        "code": EGRESS_DEFERRED_CODE,
                        "message": format!(
                            "出口熔断中（{}）：到 {until_ms} 再试",
                            resolution.as_key()
                        ),
                    }),
                )
            }
            Admission::Waiting => {
                return (
                    0,
                    json!({
                        "code": EGRESS_DEFERRED_CODE,
                        "message": format!("出口试探在飞（{}）：稍后再试", resolution.as_key()),
                    }),
                )
            }
        }
    } else {
        None
    };
    // `_permit` 必须活到响应回来：并发与最小间隔是「持有期间」计数的。
    let _permit = match admit_platform_request(product, region_code, uid).await {
        Ok(permit) => permit,
        Err(error) => {
            // 请求根本没发出去：这次半开试探必须交回。按 `Transient` 归还只释放试探槽、
            // 不推进熔断（出口本身没被证明坏），否则其余请求要白等 `PROBE_TIMEOUT_MS`。
            if let Some(key) = &egress_key {
                let _ = egress::BREAKER.record_failure(
                    key,
                    FailureClass::Transient,
                    now_ms().max(0) as u64,
                );
            }
            return (
                0,
                json!({
                    "code": ADMISSION_BLOCKED_CODE,
                    "message": format!("风控准入拦截：{error}"),
                }),
            )
        }
    };
    let headers = outbound_headers(account, region, extra_headers);
    let (status, body) =
        authed_json_request_for(region, url, method, body, account, &headers).await;
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let reason = body
        .get("message")
        .or_else(|| body.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(key) = &egress_key {
        match failure_class(status) {
            Some(class) => {
                let _ = egress::BREAKER.record_failure(key, class, now_ms().max(0) as u64);
            }
            // 2xx / 业务 4xx：出口是通的（上游的答复就是证明），清掉失败史。
            // 半开试探也在这里释放，否则其余请求会一直 `Waiting`。
            None => egress::BREAKER.record_success(key),
        }
    }
    let _ = record_platform_response(product, region_code, uid, code, reason);
    (status, body)
}

/// 组装出网头：调用方给的 feature 头 +（**开关打开时**）设备身份头。
///
/// 这是设备身份的**唯一挂载点**（`docs/specs/WB-AUTOMATION-ARCH.md` §7.1）。放在这里而不是
/// 每个 feature 模块里，是因为「关掉开关」必须是全局的：只要还有一条路自己拼 `x-client-*`，
/// 用户关了开关也仍在发旧身份。开关关着时本函数返回的就是 `extra` 的原样拷贝。
pub fn outbound_headers(
    account: &Value,
    region: Region,
    extra: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut headers = extra.clone();
    let _ = device_identity::apply(account, region, now_ms().max(0) as u64, &mut headers);
    headers
}

/// 生产实现：真实 HTTP，逐字沿用 [`authed_json_request_for`] 的鉴权 / 刷新 / 信封短路。
///
/// 顺序**不可颠倒**：准入 → 请求 → 结果回写。少了准入，风控设置对任务链就是摆设；
/// 少了回写，401/429 不会让账号暂停或冷却，下一轮还会继续打。
pub struct RealTaskIo {
    product: &'static str,
}

impl RealTaskIo {
    pub fn new(product: &'static str) -> Self {
        Self { product }
    }

    /// 任务链当前只服务 WorkBuddy（`workbuddy` + `cn` / `global`）。
    pub fn workbuddy() -> Self {
        Self { product: "workbuddy" }
    }
}

impl Default for RealTaskIo {
    fn default() -> Self {
        Self::workbuddy()
    }
}

impl std::fmt::Display for RealTaskIo {
    /// 只打产品名：账号 JSON 绝不进日志（`TaskCall` 因此没有 `Debug`）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.product)
    }
}

impl TaskIo for RealTaskIo {
    fn request<'a>(
        &'a self,
        call: TaskCall<'a>,
        method: &'a str,
        body: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = TaskReply> + Send + 'a>> {
        Box::pin(async move {
            send(
                self.product,
                call.region,
                call.url,
                method,
                body,
                call.account,
                &call.headers,
            )
            .await
        })
    }
}

/// 日志 / 诊断用的请求标签：`METHOD /path?…`，URL 里可能的凭据先脱敏。
///
/// **只用它，别自己 `format!("{call:?}")`** —— `TaskCall` 甚至没有 `Debug`，
/// 就是为了让「顺手打印整个请求」编译不过。
pub fn call_label(method: &str, url: &str) -> String {
    format!("{method} {}", redact(url))
}

/// 测试替身：不发任何请求，按脚本顺序返回预设响应，并记录每次调用。
///
/// 只在测试构建里存在（`#[cfg(test)]`）：生产代码没有任何路径能拿到它，
/// 所以「离线跑任务链」不会意外变成「线上静默不发请求」。
#[cfg(test)]
pub struct FakeTaskIo {
    replies: std::sync::Mutex<std::collections::VecDeque<TaskReply>>,
    calls: std::sync::Mutex<Vec<FakeCall>>,
}

/// [`FakeTaskIo`] 记录到的一次调用（账号只留 `uid`，不留凭据）。
///
/// 只有 `PartialEq`：`serde_json::Value` 里有 `f64`，实现不了 `Eq`。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct FakeCall {
    pub method: String,
    pub url: String,
    pub uid: String,
    pub body: Option<Value>,
    pub headers: HashMap<String, String>,
}

#[cfg(test)]
impl FakeTaskIo {
    pub fn new() -> Self {
        Self {
            replies: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 排一个响应。队列空时返回 `(404, {"code": -404})`（测试里漏排一眼就能看见）。
    pub fn reply(&self, status: u16, body: Value) {
        self.replies.lock().expect("replies").push_back((status, body));
    }

    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls.lock().expect("calls").clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("calls").len()
    }

    fn record(&self, method: &str, call: &TaskCall<'_>, body: Option<Value>) {
        self.calls.lock().expect("calls").push(FakeCall {
            method: method.to_string(),
            url: call.url.to_string(),
            uid: call
                .account
                .get("uid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            body,
            headers: call.headers.clone(),
        });
    }

    fn next_reply(&self) -> TaskReply {
        self.replies
            .lock()
            .expect("replies")
            .pop_front()
            .unwrap_or_else(|| (404, json!({"code": -404, "message": "no scripted reply"})))
    }
}

#[cfg(test)]
impl Default for FakeTaskIo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl TaskIo for FakeTaskIo {
    fn request<'a>(
        &'a self,
        call: TaskCall<'a>,
        method: &'a str,
        body: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = TaskReply> + Send + 'a>> {
        Box::pin(async move {
            self.record(method, &call, body);
            self.next_reply()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: &str, token: &str) -> Value {
        json!({ "uid": uid, "access_token": token })
    }

    /// 出口熔断只认**出口级**失败：业务 4xx 与 2xx 都不能推进熔断。
    #[test]
    fn only_egress_level_failures_reach_the_breaker() {
        assert_eq!(failure_class(0), Some(FailureClass::Transient));
        assert_eq!(failure_class(407), Some(FailureClass::Hard));
        assert_eq!(failure_class(429), Some(FailureClass::RateLimit));
        assert_eq!(failure_class(502), Some(FailureClass::Transient));
        assert_eq!(failure_class(503), Some(FailureClass::Transient));
        // 任务链上这些全是正常业务态：`400 task not completed` / `403 连登天数不足` /
        // token 过期 / 领奖已领过。记到出口上会把整条出口熔断。
        for status in [200, 201, 400, 401, 403, 404, 409, 422] {
            assert_eq!(failure_class(status), None, "status={status}");
        }
        // 只有 Hard 真的计数（`backoff` 的语义在这里再钉一次，避免有人改分类表）。
        assert!(FailureClass::Hard.counts_toward_breaker());
        assert!(!FailureClass::Transient.counts_toward_breaker());
        assert!(!FailureClass::RateLimit.counts_toward_breaker());
        assert_ne!(EGRESS_DEFERRED_CODE, ADMISSION_BLOCKED_CODE);
    }

    /// 默认关：一个没有 `automation.egress_backoff` 的账号不会被出口熔断挡住。
    #[test]
    fn egress_backoff_is_opt_in() {
        assert!(!egress::backoff_enabled(&json!({"uid": "u1"})));
        assert!(!egress::backoff_enabled(
            &json!({"uid": "u1", "automation": {"egress_backoff": false}})
        ));
        assert!(egress::backoff_enabled(
            &json!({"uid": "u1", "automation": {"egress_backoff": true}})
        ));
    }

    #[test]
    fn label_never_leaks_credentials_from_the_url() {
        let label = call_label(
            "POST",
            "https://copilot.tencent.com/v2/activity/growth/tasks?access_token=supersecret&x=1",
        );
        assert!(!label.contains("supersecret"), "{label}");
        assert!(label.starts_with("POST https://"));
        assert!(label.contains(crate::modules::runtime_log::REDACTED));
    }

    // ⚠️ 「`TaskCall` 没有 `Debug`」这条约束没法用测试表达（Rust 没有否定 trait bound），
    // 只能靠 review：给它加 `#[derive(Debug)]` 会让 `format!("{call:?}")` 编译通过，
    // 而那样等于把 access token 印进日志。改动这个类型时请回来读一遍这句。

    /// 默认关：出网头与调用方给的**逐字相同**（开关关着时行为与今日完全一致）。
    ///
    /// 刻意只测关着的路径：开着的路径要读/写安装盐文件（`install_salt`），
    /// 那部分由 `device_identity::tests` 用临时目录覆盖，避免单测碰用户主目录。
    #[test]
    fn outbound_headers_change_nothing_unless_the_account_opts_in() {
        let extra = HashMap::from([
            ("x-client-platform".to_string(), "web".to_string()),
            ("origin".to_string(), "https://www.codebuddy.cn".to_string()),
        ]);
        let plain = json!({ "uid": "uid-1", "access_token": "tok" });
        assert_eq!(outbound_headers(&plain, Region::Cn, &extra), extra);

        let explicit_off =
            json!({ "uid": "uid-1", "automation": { "device_identity": false } });
        assert_eq!(outbound_headers(&explicit_off, Region::Cn, &extra), extra);
    }

    #[tokio::test]
    async fn fake_io_routes_get_and_post_and_records_calls() {
        let io = FakeTaskIo::new();
        let account = account("uid-1", "tok");
        io.reply(200, json!({"code": 0, "data": {"tasks": []}}));
        io.reply(200, json!({"code": 0}));

        let (status, body) = io
            .get(
                TaskCall::new(Region::Cn, "https://example.test/tasks", &account)
                    .header("x-client-platform", "web"),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["code"], 0);

        let (status, _) = io
            .post(
                TaskCall::new(Region::Cn, "https://example.test/claim", &account),
                json!({}),
            )
            .await;
        assert_eq!(status, 200);

        let calls = io.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, "GET");
        assert_eq!(calls[0].body, None);
        assert_eq!(calls[0].uid, "uid-1");
        assert_eq!(calls[0].headers["x-client-platform"], "web");
        assert_eq!(calls[1].method, "POST");
        assert_eq!(calls[1].body, Some(json!({})));
    }

    #[tokio::test]
    async fn fake_io_fails_loudly_when_no_reply_is_scripted() {
        let io = FakeTaskIo::new();
        let account = account("uid-1", "tok");
        let (status, body) = io
            .get(TaskCall::new(Region::Cn, "https://example.test/tasks", &account))
            .await;
        assert_eq!(status, 404);
        assert_eq!(body["code"], -404);
    }

    #[tokio::test]
    async fn admission_block_is_distinguishable_from_an_upstream_failure() {
        // 契约测试：`status = 0` + `code = -3` 是「本地没发」，`status != 0` 才是上游。
        assert_eq!(ADMISSION_BLOCKED_CODE, -3);
        let io = FakeTaskIo::new();
        let account = account("uid-1", "tok");
        io.reply(
            0,
            json!({"code": ADMISSION_BLOCKED_CODE, "message": "风控准入拦截：account cooling"}),
        );
        let (status, body) = io
            .post(
                TaskCall::new(Region::Cn, "https://example.test/claim", &account),
                json!({}),
            )
            .await;
        assert_eq!(status, 0);
        assert_eq!(body["code"], ADMISSION_BLOCKED_CODE);
        // 错误串里不能出现 token（账号 JSON 不参与错误文案）。
        assert!(!body.to_string().contains("tok"));
    }

    /// 源码级闸门：`activity.rs` / `cat.rs` **不许**自己发请求。
    ///
    /// 用 `include_str!` 把源码带进编译产物，和代码同源：谁再写回
    /// `authed_json_request_for(` / `http_request(` / `reqwest::`，`cargo test` 立刻红。
    /// 依据 `docs/specs/WB-AUTOMATION-ARCH.md` §3.1（所有出网走 `TaskIo`）。
    #[test]
    fn task_modules_never_call_http_directly() {
        let modules = [
            ("activity.rs", include_str!("activity.rs")),
            ("cat.rs", include_str!("cat.rs")),
        ];
        for (name, source) in modules {
            for forbidden in ["authed_json_request_for(", "http_request(", "reqwest::"] {
                assert!(
                    !source.contains(forbidden),
                    "{name} 直接调用了 {forbidden}：请改走 task_io::send"
                );
            }
        }
    }
}
