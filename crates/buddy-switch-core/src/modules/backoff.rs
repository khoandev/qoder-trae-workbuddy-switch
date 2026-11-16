//! 出口（egress）层的**退避与熔断**：只管「这条线路还通不通」，**不碰账号状态**。
//!
//! 设计依据：`docs/specs/WB-AUTOMATION-ARCH.md` §7.2。三条判据决定它为什么单独成模块：
//!
//! 1. **按出口计数，不按账号**：一个代理宕掉，受影响的是**所有**走它的账号；反过来，
//!    一个账号自己撞上业务态（4xx）也不该把整条出口停掉。熔断键是 [`EgressKey`]
//!    （`slot-N` 或 `direct`），与 `risk_control` 的账号级冷却**两层并存**，
//!    调用方取更严的那一个 —— 本模块**不读也不写** `risk_control` 的任何状态。
//! 2. **只对 [`FailureClass::Hard`] 计数**：5xx / 超时 / 连接重置只是「这一次不巧」，
//!    翻倍重试即可；`429` 尊重服务端给的 `Retry-After`；只有「照样再发一次还是这个结果」
//!    的失败才推进熔断。
//! 3. **半开只放一次试探**：窗口到期后允许**一个**请求去探路，结果回来前其余请求继续等；
//!    探路失败窗口翻倍（30 分钟起，6 小时封顶），成功则整条链路的失败史清零；
//!    试探迟迟不回报（panic / future 被丢弃）超过 [`PROBE_TIMEOUT_MS`] 就当作丢了，允许再探。
//!
//! 本模块**没有任何 IO**：不读配置、不落盘、不看时钟（`now_ms` 由调用方传入），
//! 所以离线测试能精确复现每一次状态迁移。熔断状态是**进程内**的 —— 重启即归零，
//! 这正好是「用户手动重开一次」的语义；跨重启的账号级冷却仍由 `risk_control` 负责。

use std::collections::BTreeMap;
use std::sync::Mutex;

/// 指数退避的基数：第一次重试等 1s。
pub const BASE_DELAY_MS: u64 = 1_000;
/// 指数退避的上限（确定部分到顶就不再涨；抖动会让最终值最多高 25%）。
pub const MAX_DELAY_MS: u64 = 15_000;
/// 抖动比例：最终值落在确定值的 0.75–1.25 倍之间。
pub const JITTER_RATIO: f64 = 0.25;
/// 尊重 `Retry-After` 的上限：超过 5 分钟就不在一次任务里干等，
/// 交给下一轮调度（或熔断）处理。
pub const MAX_RETRY_AFTER_MS: u64 = 5 * 60_000;
/// 连续多少次 [`FailureClass::Hard`] 才熔断。
pub const HARD_FAILURE_THRESHOLD: u32 = 3;
/// 第一次熔断的窗口：30 分钟。
pub const BREAKER_BASE_MS: u64 = 30 * 60_000;
/// 熔断窗口的上限：6 小时。
pub const BREAKER_MAX_MS: u64 = 6 * 60 * 60_000;
/// 半开试探的回报期限：超过就当作丢了，允许再派一个试探。
///
/// 没有这条，一次 panic / 被丢弃的 future 会让这条出口**永久**卡在 [`Admission::Waiting`]。
pub const PROBE_TIMEOUT_MS: u64 = 60_000;
/// 直连（无槽）在熔断表里的键名。
pub const EGRESS_DIRECT: &str = "direct";

/// 一次失败该怎么修 —— **只描述重试策略，不描述账号健康**。
///
/// ⚠️ 平台把一部分 4xx 当**正常业务态**用（`growth` 域 `403` = 「连续登录天数不足」，
/// `400 task not completed` = 「还没到」）。这类响应必须在调用方就收敛成任务结果，
/// **不要**送进 [`FailureClass::from_status`]：否则三次「天数不足」会把整条出口熔断。
/// 这里只负责「出口层」的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// 这一次不巧：超时、连接重置、5xx，以及 `status = 0`（请求根本没发出去）。
    Transient,
    /// 服务端明确限速：按 `Retry-After` 等，不推进熔断。
    RateLimit,
    /// 照样再发还是这个结果：不重试本次请求，并推进出口熔断。
    Hard,
}

impl FailureClass {
    /// HTTP 状态码 → 失败类别。`0` 是本地没让发（风控准入拦截）的哨兵值。
    pub fn from_status(status: u16) -> Self {
        match status {
            0 | 408 | 425 => Self::Transient,
            429 => Self::RateLimit,
            500..=599 => Self::Transient,
            400..=499 => Self::Hard,
            _ => Self::Transient,
        }
    }

    /// 同一请求是否值得再发一次（`Hard` 不值得）。
    pub fn retryable(self) -> bool {
        !matches!(self, Self::Hard)
    }

    /// 是否计入出口熔断（只有 `Hard` 计）。
    pub fn counts_toward_breaker(self) -> bool {
        matches!(self, Self::Hard)
    }
}

/// 指数退避参数。`Copy`，方便按账号/feature 各持一份。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffPolicy {
    pub base_ms: u64,
    pub max_ms: u64,
    pub jitter_ratio: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base_ms: BASE_DELAY_MS,
            max_ms: MAX_DELAY_MS,
            jitter_ratio: JITTER_RATIO,
        }
    }
}

impl BackoffPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.base_ms == 0 {
            return Err("base_ms must be >= 1".into());
        }
        if self.max_ms < self.base_ms {
            return Err("max_ms must be >= base_ms".into());
        }
        if !self.jitter_ratio.is_finite() || !(0.0..=1.0).contains(&self.jitter_ratio) {
            return Err("jitter_ratio must be 0.0..=1.0".into());
        }
        Ok(())
    }

    /// 确定部分：`min(base * 2^attempt, max)`。
    ///
    /// `attempt` 是**已经失败的次数**（首次重试传 0）。
    pub fn base_delay_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.min(20);
        self.base_ms.saturating_mul(1u64 << shift).min(self.max_ms)
    }

    /// 叠加抖动后的等待：确定部分 × [`jitter_factor`]。
    ///
    /// `sample` ∈ `[0, 1]`，由 [`random_sample`] 提供；测试里显式传值以保证可复现。
    pub fn delay_ms(&self, attempt: u32, sample: f64) -> u64 {
        scale_ms(self.base_delay_ms(attempt), jitter_factor(self.jitter_ratio, sample))
    }

    /// 有 `Retry-After` 时以**服务端**为准（夹在 `[base_ms, MAX_RETRY_AFTER_MS]`）。
    pub fn delay_with_retry_after(
        &self,
        attempt: u32,
        sample: f64,
        retry_after_ms: Option<u64>,
    ) -> u64 {
        match retry_after_ms {
            Some(raw) => clamp_retry_after(raw, self.base_ms),
            None => self.delay_ms(attempt, sample),
        }
    }
}

/// 抖动系数：`sample = 0` → `1 - ratio`，`sample = 1` → `1 + ratio`。
pub fn jitter_factor(ratio: f64, sample: f64) -> f64 {
    let sample = sample.clamp(0.0, 1.0);
    let ratio = ratio.max(0.0);
    1.0 + ratio * (2.0 * sample - 1.0)
}

/// `Retry-After` 的毫秒值夹到 `[base_ms, MAX_RETRY_AFTER_MS]`。
///
/// 下界防「服务端说 0 秒」把退避变成忙等；上界防一个 `Retry-After: 86400` 把工作线程钉死一天。
pub fn clamp_retry_after(raw_ms: u64, base_ms: u64) -> u64 {
    let floor = base_ms.min(MAX_RETRY_AFTER_MS);
    raw_ms.clamp(floor, MAX_RETRY_AFTER_MS)
}

/// 解析 `Retry-After` 头的**秒**形态，返回毫秒。
///
/// 只认数字秒；HTTP-date 形态返回 `None`（那需要时钟对齐，本模块刻意不碰时钟）——
/// 调用方拿不到就用指数退避。
pub fn parse_retry_after(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds.saturating_mul(1_000))
}

/// 均匀分布在 `[0, 1)` 的抖动样本。
///
/// 刻意用 `uuid::Uuid::new_v4()` 取随机位，而不是引入 `rand`：本仓已依赖 `uuid`，
/// 抖动不需要密码学强度，只需要「两个进程不同步」。
pub fn random_sample() -> f64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[..8]);
    (u64::from_le_bytes(raw) as f64) / (u64::MAX as f64 + 1.0)
}

fn scale_ms(ms: u64, factor: f64) -> u64 {
    if !factor.is_finite() || factor <= 0.0 {
        return 0;
    }
    (ms as f64 * factor).round() as u64
}

/// 熔断的键：**一条出口链路**，不是账号。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EgressKey {
    /// 直连 / 系统代理（没有绑定槽）。
    Direct,
    /// 用户命名的代理槽，如 `slot-1`。
    Slot(String),
}

impl EgressKey {
    pub fn direct() -> Self {
        Self::Direct
    }

    /// 构造一个槽键。槽 id 单调分配、不复用（见 `WB-AUTOMATION-ARCH.md` §7.2）。
    pub fn slot(id: impl AsRef<str>) -> Result<Self, String> {
        let id = id.as_ref().trim();
        if id.is_empty() || id.len() > 64 {
            return Err("egress slot id must contain 1..=64 characters".into());
        }
        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err("egress slot id may contain only ASCII letters, digits, '-' and '_'".into());
        }
        if id == EGRESS_DIRECT {
            return Err(format!("egress slot id must not collide with '{EGRESS_DIRECT}'"));
        }
        Ok(Self::Slot(id.to_string()))
    }

    /// 落进熔断表的字符串键。
    pub fn as_key(&self) -> &str {
        match self {
            Self::Direct => EGRESS_DIRECT,
            Self::Slot(id) => id,
        }
    }
}

/// 熔断参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BreakerConfig {
    pub hard_failure_threshold: u32,
    pub base_open_ms: u64,
    pub max_open_ms: u64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            hard_failure_threshold: HARD_FAILURE_THRESHOLD,
            base_open_ms: BREAKER_BASE_MS,
            max_open_ms: BREAKER_MAX_MS,
        }
    }
}

/// 一条出口的熔断状态。持久化不做，重启动即归零（见模块头）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakerEntry {
    /// 距上次成功以来的连续 `Hard` 失败次数（到阈值就开）。
    pub consecutive_hard_failures: u32,
    /// 累计熔断次数（决定窗口翻几番）。
    pub openings: u32,
    /// 窗口结束时刻；`0` = 从未熔断过。
    pub open_until_ms: u64,
    /// 最近一次 `Hard` 失败的时刻（账号页排障用）。
    pub last_hard_failure_ms: u64,
    /// 半开试探已发出、结果未回。
    pub probe_in_flight: bool,
    /// 试探发出的时刻（配合 [`PROBE_TIMEOUT_MS`] 判定试探是否丢了）。
    pub probe_started_ms: u64,
}

impl BreakerEntry {
    /// 窗口内（含）—— 请求必须等。
    pub fn is_open(&self, now_ms: u64) -> bool {
        self.open_until_ms > now_ms
    }

    /// 窗口已过、且这条出口熔断过 —— 只放一次试探。
    pub fn is_half_open(&self, now_ms: u64) -> bool {
        self.openings > 0 && self.open_until_ms != 0 && self.open_until_ms <= now_ms
    }
}

/// [`EgressBreaker::admit`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 正常放行。
    Allow,
    /// 半开：这是唯一一次试探，结果必须回报 [`EgressBreaker::record_failure`] 或
    /// [`EgressBreaker::record_success`]，否则其余请求会一直等。
    Probe,
    /// 试探在飞，等它出结果。
    Waiting,
    /// 熔断中，到这个时刻再试。
    Defer { until_ms: u64 },
}

/// 出口级熔断器：**纯内存**、按 [`EgressKey`] 隔离、线程安全。
pub struct EgressBreaker {
    config: BreakerConfig,
    entries: Mutex<BTreeMap<String, BreakerEntry>>,
}

impl EgressBreaker {
    pub fn new(config: BreakerConfig) -> Result<Self, String> {
        if config.hard_failure_threshold == 0 {
            return Err("hard_failure_threshold must be >= 1".into());
        }
        if config.base_open_ms == 0 {
            return Err("base_open_ms must be >= 1".into());
        }
        if config.max_open_ms < config.base_open_ms {
            return Err("max_open_ms must be >= base_open_ms".into());
        }
        Ok(Self {
            config,
            entries: Mutex::new(BTreeMap::new()),
        })
    }

    /// 生产默认值：3 次 `Hard` → 30 分钟，翻倍，6 小时封顶。
    pub fn with_defaults() -> Self {
        Self::new(BreakerConfig::default()).expect("默认熔断配置合法")
    }

    pub fn config(&self) -> BreakerConfig {
        self.config
    }

    /// 这条出口现在能不能发请求。
    ///
    /// 会**改状态**：半开时把 `probe_in_flight` 置位，所以调用方拿到 [`Admission::Probe`]
    /// 就必须回报结果（成功或失败）。
    pub fn admit(&self, egress: &EgressKey, now_ms: u64) -> Admission {
        let mut entries = self.entries.lock().expect("egress breaker poisoned");
        let entry = entries.entry(egress.as_key().to_string()).or_default();
        if entry.open_until_ms == 0 {
            return Admission::Allow;
        }
        if now_ms < entry.open_until_ms {
            return Admission::Defer {
                until_ms: entry.open_until_ms,
            };
        }
        if entry.probe_in_flight {
            let deadline = entry.probe_started_ms.saturating_add(PROBE_TIMEOUT_MS);
            if now_ms < deadline {
                return Admission::Waiting;
            }
            // 试探超时没回报 ⇒ 当作丢了：再派一个，否则半开会被永久占住。
        }
        entry.probe_in_flight = true;
        entry.probe_started_ms = now_ms;
        Admission::Probe
    }

    /// 记录一次失败。只有 [`FailureClass::Hard`] 推进熔断。
    ///
    /// 返回 `Some(until_ms)` 表示**这一次**把出口熔断了（调用方据此写日志 / 打点）。
    pub fn record_failure(
        &self,
        egress: &EgressKey,
        class: FailureClass,
        now_ms: u64,
    ) -> Option<u64> {
        let mut entries = self.entries.lock().expect("egress breaker poisoned");
        let entry = entries.entry(egress.as_key().to_string()).or_default();
        let probe_failed = entry.probe_in_flight;
        // 试探槽必须先释放：无论这次失败是哪一类，都不能让下一个请求永远 `Waiting`。
        entry.probe_in_flight = false;
        if !class.counts_toward_breaker() {
            // transient / rate_limit 只说明「这一次没成」，不推进熔断；出口仍留在半开，
            // 下一次请求可以重新试探。
            return None;
        }
        let was_open = entry.is_open(now_ms);
        entry.consecutive_hard_failures = entry.consecutive_hard_failures.saturating_add(1);
        entry.last_hard_failure_ms = now_ms;
        // 窗口内的迟到失败只记账：不重复翻倍窗口，否则一次抖动能把窗口推到上限。
        if was_open && !probe_failed {
            return None;
        }
        // 半开试探失败 ⇒ 立刻重开（不必再攒够阈值）；否则要攒到阈值。
        if !probe_failed && entry.consecutive_hard_failures < self.config.hard_failure_threshold {
            return None;
        }
        entry.openings = entry.openings.saturating_add(1);
        entry.consecutive_hard_failures = 0;
        let window = self.window_ms(entry.openings);
        entry.open_until_ms = now_ms.saturating_add(window);
        Some(entry.open_until_ms)
    }

    /// 记录一次成功：清掉失败史与熔断窗口（Escalation 也从头算）。
    pub fn record_success(&self, egress: &EgressKey) {
        let mut entries = self.entries.lock().expect("egress breaker poisoned");
        entries.insert(egress.as_key().to_string(), BreakerEntry::default());
    }

    /// 这条出口的当前状态快照（不存在 ⇒ 默认值）。
    pub fn entry(&self, egress: &EgressKey) -> BreakerEntry {
        self.entries
            .lock()
            .expect("egress breaker poisoned")
            .get(egress.as_key())
            .cloned()
            .unwrap_or_default()
    }

    /// 还在熔断窗口里 ⇒ `Some(结束时刻)`；否则 `None`。
    pub fn open_until(&self, egress: &EgressKey) -> Option<u64> {
        let entries = self.entries.lock().expect("egress breaker poisoned");
        entries
            .get(egress.as_key())
            .map(|entry| entry.open_until_ms)
            .filter(|until| *until != 0)
    }

    /// 第 `openings` 次熔断的窗口：`min(base * 2^(openings-1), max)`。
    fn window_ms(&self, openings: u32) -> u64 {
        let shift = openings.saturating_sub(1).min(20);
        self.config.base_open_ms.saturating_mul(1u64 << shift).min(self.config.max_open_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_763_270_400_000;

    #[test]
    fn classification_matches_the_retry_policy() {
        // `0` = 风控准入拦截，请求根本没发出去；重试是安全的。
        assert_eq!(FailureClass::from_status(0), FailureClass::Transient);
        assert_eq!(FailureClass::from_status(408), FailureClass::Transient);
        assert_eq!(FailureClass::from_status(425), FailureClass::Transient);
        assert_eq!(FailureClass::from_status(502), FailureClass::Transient);
        assert_eq!(FailureClass::from_status(429), FailureClass::RateLimit);
        assert_eq!(FailureClass::from_status(400), FailureClass::Hard);
        assert_eq!(FailureClass::from_status(401), FailureClass::Hard);
        assert_eq!(FailureClass::from_status(403), FailureClass::Hard);
        assert_eq!(FailureClass::from_status(451), FailureClass::Hard);

        assert!(FailureClass::Transient.retryable());
        assert!(FailureClass::RateLimit.retryable());
        assert!(!FailureClass::Hard.retryable());
        assert!(FailureClass::Hard.counts_toward_breaker());
        assert!(!FailureClass::Transient.counts_toward_breaker());
        assert!(!FailureClass::RateLimit.counts_toward_breaker());
    }

    #[test]
    fn backoff_doubles_from_one_second_and_caps_at_fifteen() {
        let policy = BackoffPolicy::default();
        assert_eq!(policy.base_delay_ms(0), BASE_DELAY_MS);
        assert_eq!(policy.base_delay_ms(1), 2_000);
        assert_eq!(policy.base_delay_ms(2), 4_000);
        assert_eq!(policy.base_delay_ms(3), 8_000);
        // 16s 被 cap 到 15s。
        assert_eq!(policy.base_delay_ms(4), MAX_DELAY_MS);
        assert_eq!(policy.base_delay_ms(9), MAX_DELAY_MS);
        // 采样 0.5 ⇒ 抖动系数 1.0，最终值就是确定值。
        assert_eq!(policy.delay_ms(2, 0.5), 4_000);
        // 尝试次数再大也不会溢出。
        assert_eq!(policy.base_delay_ms(u32::MAX), MAX_DELAY_MS);
    }

    #[test]
    fn jitter_stays_inside_the_documented_band() {
        let policy = BackoffPolicy::default();
        assert_eq!(policy.delay_ms(0, 0.0), 750);
        assert_eq!(policy.delay_ms(0, 0.5), 1_000);
        assert_eq!(policy.delay_ms(0, 1.0), 1_250);
        // 抖动不改单调性：尝试越多，等得越久（同一样本下）。
        for attempt in 0..6u32 {
            assert!(policy.delay_ms(attempt, 0.5) <= policy.delay_ms(attempt + 1, 0.5));
        }
        // 样本越界被夹住，不会放大成 2 倍。
        assert_eq!(jitter_factor(JITTER_RATIO, -3.0), 0.75);
        assert_eq!(jitter_factor(JITTER_RATIO, 7.0), 1.25);
    }

    #[test]
    fn backoff_never_exceeds_the_cap_plus_jitter() {
        let policy = BackoffPolicy::default();
        for attempt in 0..32u32 {
            let delay = policy.delay_ms(attempt, 1.0);
            assert!(delay <= MAX_DELAY_MS + MAX_DELAY_MS / 4, "attempt {attempt}: {delay}");
        }
    }

    #[test]
    fn retry_after_is_parsed_and_clamped() {
        assert_eq!(parse_retry_after("120"), Some(120_000));
        assert_eq!(parse_retry_after(" 0 "), Some(0));
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("-5"), None);
        // HTTP-date 形态不解析（需要时钟对齐，本模块刻意不碰时钟）。
        assert_eq!(parse_retry_after("Tue, 18 Nov 2026 07:00:00 GMT"), None);

        let policy = BackoffPolicy::default();
        assert_eq!(policy.delay_with_retry_after(0, 0.5, Some(60_000)), 60_000);
        // 服务端说 0 秒 ⇒ 至少等 base（不要忙等）。
        assert_eq!(policy.delay_with_retry_after(0, 0.5, Some(0)), BASE_DELAY_MS);
        // 服务端说 1 小时 ⇒ 夹到上限。
        assert_eq!(policy.delay_with_retry_after(0, 0.5, Some(3_600_000)), MAX_RETRY_AFTER_MS);
        // 没有 Retry-After ⇒ 走指数退避。
        assert_eq!(policy.delay_with_retry_after(3, 0.5, None), policy.delay_ms(3, 0.5));
    }

    #[test]
    fn three_hard_failures_open_the_slot_for_thirty_minutes() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        assert_eq!(breaker.admit(&slot, NOW), Admission::Allow);
        assert_eq!(breaker.record_failure(&slot, FailureClass::Hard, NOW), None);
        assert_eq!(breaker.record_failure(&slot, FailureClass::Hard, NOW), None);
        // 阈值是 3：前两次都不熔断。
        assert_eq!(breaker.admit(&slot, NOW), Admission::Allow);
        let opened = breaker.record_failure(&slot, FailureClass::Hard, NOW);
        let until = opened.expect("第三次必须熔断");
        assert_eq!(until, NOW + BREAKER_BASE_MS);
        assert_eq!(breaker.admit(&slot, NOW + 1), Admission::Defer { until_ms: until });
        // 窗口结束前一律不放行，哪怕换个账号也一样 —— 键是出口。
        assert_eq!(breaker.admit(&slot, until - 1), Admission::Defer { until_ms: until });
    }

    #[test]
    fn open_window_doubles_up_to_six_hours() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        let mut now = NOW;
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, now);
        }
        let mut until = breaker.open_until(&slot).expect("阈值到了就该熔断");
        let mut windows = vec![until - now];
        for _ in 0..5 {
            // 窗口过期 ⇒ 半开，只放一次试探；试探失败立刻重开并翻倍。
            now = until + 1;
            assert_eq!(breaker.admit(&slot, now), Admission::Probe);
            let reopened = breaker.record_failure(&slot, FailureClass::Hard, now);
            until = reopened.expect("试探失败必须立刻重开");
            windows.push(until - now);
        }
        assert_eq!(
            windows,
            vec![
                BREAKER_BASE_MS,
                BREAKER_BASE_MS * 2,
                BREAKER_BASE_MS * 4,
                BREAKER_BASE_MS * 8,
                BREAKER_BASE_MS * 12,
                BREAKER_MAX_MS,
            ]
        );
        assert_eq!(BREAKER_MAX_MS, 6 * 60 * 60_000);
    }

    #[test]
    fn half_open_allows_exactly_one_probe() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, NOW);
        }
        let until = breaker.open_until(&slot).unwrap();
        assert_eq!(breaker.admit(&slot, until), Admission::Probe);
        // 第二个并发请求必须等试探结果，不能一起冲。
        assert_eq!(breaker.admit(&slot, until), Admission::Waiting);
        breaker.record_success(&slot);
        assert_eq!(breaker.admit(&slot, until), Admission::Allow);
        assert_eq!(breaker.open_until(&slot), None);
        assert_eq!(breaker.entry(&slot), BreakerEntry::default());
    }

    #[test]
    fn success_resets_the_escalation_ladder() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, NOW);
        }
        let until = breaker.open_until(&slot).unwrap();
        assert_eq!(breaker.admit(&slot, until), Admission::Probe);
        breaker.record_success(&slot);

        // 清零后重新熔断，窗口从 30 分钟重新开始，而不是接着上一次翻倍。
        let later = until + 1;
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, later);
        }
        let second = breaker.open_until(&slot).expect("必须再次熔断");
        assert_eq!(second - later, BREAKER_BASE_MS);
    }

    #[test]
    fn transient_and_rate_limit_failures_never_open_the_breaker() {
        let breaker = EgressBreaker::with_defaults();
        let direct = EgressKey::direct();
        for _ in 0..10 {
            assert_eq!(
                breaker.record_failure(&direct, FailureClass::Transient, NOW),
                None
            );
            assert_eq!(
                breaker.record_failure(&direct, FailureClass::RateLimit, NOW),
                None
            );
        }
        assert_eq!(breaker.open_until(&direct), None);
        assert_eq!(breaker.entry(&direct).consecutive_hard_failures, 0);
        assert_eq!(breaker.admit(&direct, NOW), Admission::Allow);
    }

    #[test]
    fn transient_probe_result_releases_the_probe_slot() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, NOW);
        }
        let until = breaker.open_until(&slot).unwrap();
        assert_eq!(breaker.admit(&slot, until), Admission::Probe);
        // 试探这次是 transient（线路通了、上游 5xx）：不推进熔断，也不占住试探槽。
        let outcome = breaker.record_failure(&slot, FailureClass::Transient, until);
        assert_eq!(outcome, None);
        assert_eq!(breaker.open_until(&slot), Some(until));
        assert_eq!(breaker.admit(&slot, until), Admission::Probe);
    }

    #[test]
    fn a_lost_probe_does_not_block_the_slot_forever() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, NOW);
        }
        let until = breaker.open_until(&slot).unwrap();
        assert_eq!(breaker.admit(&slot, until), Admission::Probe);
        // 试探的请求没回来（panic / future 被丢弃）：超时前继续等，超时后必须能再探。
        let late = until + PROBE_TIMEOUT_MS - 1;
        assert_eq!(breaker.admit(&slot, late), Admission::Waiting);
        let expired = until + PROBE_TIMEOUT_MS;
        assert_eq!(breaker.admit(&slot, expired), Admission::Probe);
    }

    #[test]
    fn late_failures_inside_an_open_window_do_not_double_it() {
        let breaker = EgressBreaker::with_defaults();
        let slot = EgressKey::slot("slot-1").unwrap();
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&slot, FailureClass::Hard, NOW);
        }
        let until = breaker.open_until(&slot).unwrap();
        // 熔断之后才回来的失败（在飞请求）只记账，不翻倍窗口。
        assert_eq!(breaker.record_failure(&slot, FailureClass::Hard, until - 1), None);
        assert_eq!(breaker.open_until(&slot), Some(until));
        assert_eq!(breaker.entry(&slot).openings, 1);
    }

    #[test]
    fn one_slot_tripping_does_not_block_other_egress() {
        let breaker = EgressBreaker::with_defaults();
        let tripped = EgressKey::slot("slot-2").unwrap();
        let healthy = EgressKey::slot("slot-3").unwrap();
        let direct = EgressKey::direct();
        for _ in 0..HARD_FAILURE_THRESHOLD {
            breaker.record_failure(&tripped, FailureClass::Hard, NOW);
        }
        assert!(breaker.open_until(&tripped).is_some());
        // 同一条链路上的其它出口、以及直连，都不受影响。
        assert_eq!(breaker.admit(&healthy, NOW), Admission::Allow);
        assert_eq!(breaker.admit(&direct, NOW), Admission::Allow);
        assert_eq!(breaker.open_until(&healthy), None);
        assert_eq!(breaker.open_until(&direct), None);
    }

    #[test]
    fn egress_key_rejects_empty_whitespace_and_direct_collisions() {
        assert!(EgressKey::slot("").is_err());
        assert!(EgressKey::slot("   ").is_err());
        assert!(EgressKey::slot(EGRESS_DIRECT).is_err());
        assert!(EgressKey::slot("slot 1").is_err());
        assert!(EgressKey::slot("a/b").is_err());
        assert_eq!(EgressKey::slot(" slot-1 ").unwrap().as_key(), "slot-1");
        assert_eq!(EgressKey::direct().as_key(), EGRESS_DIRECT);
    }

    #[test]
    fn policy_and_breaker_config_are_validated() {
        assert!(BackoffPolicy::default().validate().is_ok());

        let no_base = BackoffPolicy { base_ms: 0, ..BackoffPolicy::default() };
        assert!(no_base.validate().is_err());

        let inverted = BackoffPolicy { max_ms: 1, ..BackoffPolicy::default() };
        assert!(inverted.validate().is_err());

        let nan = BackoffPolicy { jitter_ratio: f64::NAN, ..BackoffPolicy::default() };
        assert!(nan.validate().is_err());

        let no_threshold = BreakerConfig {
            hard_failure_threshold: 0,
            ..BreakerConfig::default()
        };
        assert!(EgressBreaker::new(no_threshold).is_err());

        let no_window = BreakerConfig { base_open_ms: 0, ..BreakerConfig::default() };
        assert!(EgressBreaker::new(no_window).is_err());

        let inverted_window = BreakerConfig {
            base_open_ms: BREAKER_MAX_MS,
            max_open_ms: BREAKER_BASE_MS,
            ..BreakerConfig::default()
        };
        assert!(EgressBreaker::new(inverted_window).is_err());
    }

    #[test]
    fn random_sample_is_a_unit_interval_value() {
        for _ in 0..64 {
            let sample = random_sample();
            assert!((0.0..1.0).contains(&sample), "sample out of range: {sample}");
            let factor = jitter_factor(JITTER_RATIO, sample);
            assert!((0.75..1.25).contains(&factor), "factor out of range: {factor}");
        }
    }
}
