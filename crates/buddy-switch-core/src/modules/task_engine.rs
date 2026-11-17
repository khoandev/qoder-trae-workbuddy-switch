//! WorkBuddy 成长任务引擎：**读清单 → 排序 → 批量接取 → 复读 → 只重试一次**。
//!
//! 依据：`docs/specs/WB-AUTOMATION-ARCH.md` §3 / §3.6；参考基线 `wb2api@22b7966` 的
//! `wb_tasks.py`（`fetch_growth_tasks:255`、`accept_tasks:359`、`run_growth_tasks:1821`）。
//!
//! 五句话：
//!
//! 1. **一条流水线**：`GET /v2/activity/growth/tasks` → 排序（auto 类先）→ 每 **20** 条一批
//!    `POST …/tasks/accept` → 复读清单 → 只对仍未接上的码**再试一次**（[`MAX_ACCEPT_ROUNDS`]）。
//!    第二轮之后还接不上就留给下一轮，不硬打（参考实现同样只重试一次）。
//! 2. **auto 类不接取**：上游对 `task_type == "auto"`（如 `first_buddy`）明确
//!    「task does not require acceptance」，把它们塞进 accept 只会换来一条失败结果。
//!    它们只出现在 [`AcceptPlan::auto_first`] 里，交给后续事件链推进。
//! 3. **`first_buddy` 是 single 与猫猫旅行的前置**（上游 `prerequisite not met: first_buddy`）。
//!    没它就一律不接 single（[`SkipReason::PrerequisiteFirstBuddy`]），免得白占接取名额。
//! 4. **每次写请求前后都落台账**：动手前 `record_action`（进程崩了也留痕），失败后
//!    `record_failure`（脱敏 + 当日 3 次上限）；`needs_human` 在本轮开头就写进台账，
//!    下一轮直接跳过。键是 `{product}/{region}/{account}/{feature}/{date}`。
//! 5. **间隔取更严的那个**：`max(引擎 1s, 风控 min_gap)` 再叠 ±25% 抖动，但**不低于 1s**
//!    （上游按行为计时，见 spec §2.2 #18）。抖动是确定性的（同一 `seed` 同一结果），
//!    这样边界能在测试里钉死，不需要「大概不超 25%」这种模糊断言。
//!
//! 明确**不做**的事：上报事件与领奖不在本步（它们在事件链 + verify-before-claim 那边）。
//! 本模块只负责「该接的都接上，且不浪费名额」。
//!
//! 与参考实现的一处**有意差异**：`run_growth_tasks` 把夜间任务整体排除（`black_cat` 另有
//! `run_night_growth`）；这里允许「窗口内」接夜间任务、窗口外跳过
//! （[`SkipReason::NightOutsideWindow`]）。窗口判据仍是 `cst::in_night_window`，没有第二份。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use crate::modules::account::get_str;
use crate::modules::cst::in_night_window;
use crate::modules::outbound_headers;
use crate::modules::region::{region_spec, Region};
use crate::modules::task_catalog::{self, Gate, RawTask, ResolvedTask};
use crate::modules::task_io::{TaskCall, TaskIo, WORKBUDDY_PRODUCT};
use crate::modules::task_ledger::{
    sanitize_error, TaskDayEntry, TaskDayKey, TaskLedger, MAX_ATTEMPTS_PER_DAY,
};

/// 台账里的 feature 名（`growth_tasks`，与 spec §3.3 的开关同名）。
pub const GROWTH_FEATURE: &str = "growth_tasks";
/// 清单端点（桌面/chat 域）。
pub const TASKS_PATH: &str = "/v2/activity/growth/tasks";
/// 批量接取端点（注意 claim 走的是 `/activity/...`，没有 `/v2` —— 那是下一步的事）。
pub const ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";
/// 上游单批上限（参考实现 `chunk=20`）。
pub const ACCEPT_CHUNK: usize = 20;
/// 引擎自带的最小间隔：风控关掉时它是**唯一**的闸。
pub const ENGINE_GAP_MS: u64 = 1_000;
/// 间隔抖动幅度（±25%）。
pub const GAP_JITTER_PCT: u64 = 25;
/// 接取最多两轮：首轮 + 重试一次。
pub const MAX_ACCEPT_ROUNDS: usize = 2;
/// 所有 single 任务与猫猫旅行的前置任务码。
pub const FIRST_BUDDY: &str = "first_buddy";

// ---------------------------------------------------------------------------
// 清单
// ---------------------------------------------------------------------------

/// 服务端的 `task_type`：`auto` 不接受接取，`single` 需要先接取。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskClass {
    /// `task_type == "auto"`：上游不接受接取，事件可直接点亮。
    Auto,
    /// `task_type == "single"`：必须先接取，否则上报不计进度。
    Single,
    /// 上游新出现的类别：**不猜**，只按 `single` 之外的保守处理（不接、不报）。
    Other(String),
}

impl TaskClass {
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "auto" => Self::Auto,
            "single" => Self::Single,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Single => "single",
            Self::Other(raw) => raw,
        }
    }
}

/// 服务端的 `accept_status`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptStatus {
    /// 还没接 —— 只有这一种能进 accept 批次。
    NotAccepted,
    /// `accepted` / `already_accepted` / `claimed`：不需要再接。
    Accepted,
    /// 其它取值（上游新增状态）：当作「已经接过」，不去打接口。
    Other(String),
}

impl AcceptStatus {
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "" | "not_accepted" | "notAccepted" => Self::NotAccepted,
            "accepted" | "already_accepted" | "alreadyAccepted" | "claimed" => Self::Accepted,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_accepted(&self) -> bool {
        !matches!(self, Self::NotAccepted)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::NotAccepted => "not_accepted",
            Self::Accepted => "accepted",
            Self::Other(raw) => raw,
        }
    }
}

/// 清单里的一条任务（服务端字段优先，语义由 [`task_catalog`] 兜底）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrowthTask {
    pub code: String,
    pub description: String,
    pub jump_url: String,
    pub class: TaskClass,
    pub status: AcceptStatus,
    /// 服务端 `progress.current`。
    pub current: i64,
    /// 服务端 `progress.target`（缺省取目录值）。
    pub target: i64,
    /// 服务端 `reward_credit`（正数时优先于目录值）。
    pub reward_credit: i64,
    pub reward_energy: i64,
    /// 目录解析结果：`kind` / 三个旗标 / 专家对。
    pub spec: ResolvedTask,
}

impl GrowthTask {
    /// 进度是否已达目标（`target <= 0` 视为「没有进度可判」）。
    pub fn is_complete(&self) -> bool {
        self.target > 0 && self.current >= self.target
    }

    /// 是否需要接取。
    pub fn needs_accept(&self) -> bool {
        self.status == AcceptStatus::NotAccepted
    }
}

/// 解析清单响应：`{"code":0,"data":{"tasks":[{…}]}}`。
///
/// 字段名逐字对齐参考实现的 `fetch_growth_tasks`：`task_code` / `title` / `description`
/// （`task_desc` 兜底）/ `jump_url` / `accept_status` / `task_type` / `progress.{current,target}` /
/// `reward_credit` / `reward_energy`。**没有任务码的行直接丢掉**（没有码就没法接、没法报）。
pub fn parse_list(body: &Value) -> Vec<GrowthTask> {
    let raw_tasks = body
        .pointer("/data/tasks")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    raw_tasks.iter().filter_map(parse_task).collect()
}

fn parse_task(raw: &Value) -> Option<GrowthTask> {
    let code = raw.get("task_code").and_then(Value::as_str)?.trim();
    if code.is_empty() {
        return None;
    }
    let progress = raw.get("progress");
    let target = progress.and_then(|p| p.get("target")).and_then(Value::as_i64);
    // 参考实现是 `reward_credit or spec.reward`：0 与缺省同义，不能让 0 覆盖目录值。
    let reward = raw
        .get("reward_credit")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0);
    let spec = task_catalog::resolve(&RawTask {
        code,
        title: raw.get("title").and_then(Value::as_str),
        jump_url: raw.get("jump_url").and_then(Value::as_str),
        target,
        reward,
    });
    Some(GrowthTask {
        code: code.to_string(),
        description: raw
            .get("description")
            .or_else(|| raw.get("task_desc"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        jump_url: raw
            .get("jump_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        class: TaskClass::parse(raw.get("task_type").and_then(Value::as_str).unwrap_or("")),
        status: AcceptStatus::parse(
            raw.get("accept_status")
                .and_then(Value::as_str)
                .unwrap_or("not_accepted"),
        ),
        current: progress
            .and_then(|p| p.get("current"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        target: spec.target,
        reward_credit: spec.reward,
        reward_energy: raw
            .get("reward_energy")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        spec,
    })
}

/// 本轮的**执行顺序**：auto 类先（它们是 single 的前置），其余保持服务端顺序。
///
/// 参考实现是两趟循环（`run_growth_tasks` 第 0 步 auto、第 1 步 accept、第 2 步其余），
/// 所以这里不改顺序、只分两段：**顺序稳定**才能让「先点亮 first_buddy 再接 single」成立。
pub fn order_for_run(tasks: &[GrowthTask]) -> Vec<&GrowthTask> {
    let mut ordered: Vec<&GrowthTask> = tasks.iter().filter(|task| task.class.is_auto()).collect();
    ordered.extend(tasks.iter().filter(|task| !task.class.is_auto()));
    ordered
}

// ---------------------------------------------------------------------------
// 计划（纯函数）
// ---------------------------------------------------------------------------

/// 一条任务为什么没进接取批次。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// 今天已经领过奖（台账 `claimed`）。
    AlreadyClaimed,
    /// 今天已判「需真人」（台账 `needs_human`）。
    NeedsHuman,
    /// 当日失败次数到顶。
    AttemptsExhausted,
    /// auto 类：上游不接受接取，走事件链。
    AutoNoAccept,
    /// 已接取 / 已领奖，不需要再接。
    AlreadyAccepted,
    /// 夜间任务且当前不在 CST 23:00–08:00。
    NightOutsideWindow,
    /// `first_buddy` 未完成，single 与旅行都要等。
    PrerequisiteFirstBuddy,
    /// 目录不认识、也推不出事件链：不猜 kind，**不接**（接了也报不出有效事件）。
    UnknownCode,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyClaimed => "already_claimed",
            Self::NeedsHuman => "needs_human",
            Self::AttemptsExhausted => "attempts_exhausted",
            Self::AutoNoAccept => "auto_no_accept",
            Self::AlreadyAccepted => "already_accepted",
            Self::NightOutsideWindow => "night_outside_window",
            Self::PrerequisiteFirstBuddy => "prerequisite_first_buddy",
            Self::UnknownCode => "unknown_code",
        }
    }
}

/// 「这条没做」的一条记录（台账与账号页共用，**不含凭据**）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skipped {
    pub code: String,
    pub reason: SkipReason,
    /// 人读的一句话。
    pub detail: String,
}

impl Skipped {
    fn new(code: &str, reason: SkipReason, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            reason,
            detail: detail.into(),
        }
    }
}

/// 一轮接取的**计划**（纯函数产物，可直接断言）。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AcceptPlan {
    /// auto 类任务（不需要接取，事件链优先点亮），按执行顺序。
    pub auto_first: Vec<String>,
    /// 接取批次：每批 ≤ [`ACCEPT_CHUNK`]。
    pub batches: Vec<Vec<String>>,
    /// 本轮明确不做的（含 auto 类）。
    pub skipped: Vec<Skipped>,
    /// 本轮要标 `needs_human` 的（code + 理由）。
    pub needs_human: Vec<Skipped>,
    /// 前置未满足时是 `Some("first_buddy")`：single/旅行一律不接。
    pub blocked_by: Option<String>,
}

impl AcceptPlan {
    /// 计划要接的全部任务码（按批次顺序拍平）。
    pub fn codes(&self) -> Vec<&str> {
        self.batches
            .iter()
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// 没有要接的、也没有要推的事件 ⇒ 这一轮无事可做。
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty() && self.auto_first.is_empty()
    }
}

/// `first_buddy` 是否已经完成（single 任务与猫猫旅行的前置）。
///
/// 三种判据，任一成立即视为就绪：
/// 1. 台账里今天已经领过 `first_buddy`；
/// 2. 清单里它有进度（`current >= target`）或状态已是 `accepted` / `claimed`；
/// 3. **清单里根本没有它** —— 只把「清单里有、但还没成」当作未就绪。
///
/// 第 3 条是有意的：清单只列「还能做的任务」，老账号不会一直带着 `first_buddy`。
/// 若把「不在清单里」当成未就绪，这些账号会**永久**做不了 single 任务。
pub fn first_buddy_ready(tasks: &[GrowthTask], entry: &TaskDayEntry) -> bool {
    if entry.claimed.contains_key(FIRST_BUDDY) {
        return true;
    }
    match tasks.iter().find(|task| task.code == FIRST_BUDDY) {
        Some(task) => task.is_complete() || task.status.is_accepted(),
        None => true,
    }
}

/// 从清单 + 当日台账算出「接什么、不接什么」。
///
/// 判定顺序（先便宜、先硬的）：夜间窗口 → 需真人 → 台账（已领 / 已判真人 / 失败到顶）
/// → 未知码 → auto 类 → 已接取 → `first_buddy` 前置 → 进批次。
pub fn plan_accept(tasks: &[GrowthTask], entry: &TaskDayEntry, now_ms: i64) -> AcceptPlan {
    let mut plan = AcceptPlan::default();
    let night_open = in_night_window(now_ms);
    let buddy_ready = first_buddy_ready(tasks, entry);
    if !buddy_ready {
        plan.blocked_by = Some(FIRST_BUDDY.to_string());
    }
    let exhausted = entry.attempts >= MAX_ATTEMPTS_PER_DAY;

    let mut codes: Vec<String> = Vec::new();
    for task in order_for_run(tasks) {
        let code = task.code.as_str();
        if task.spec.night_only && !night_open {
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::NightOutsideWindow,
                "夜间任务（CST 23:00–08:00）当前不在窗口内",
            ));
            continue;
        }
        if task.spec.needs_human() {
            let reason = task.spec.needs_human_reason().unwrap_or("需要真人操作");
            plan.needs_human
                .push(Skipped::new(code, SkipReason::NeedsHuman, reason));
            continue;
        }
        if entry.claimed.contains_key(code) {
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::AlreadyClaimed,
                "今天已经领过奖",
            ));
            continue;
        }
        if let Some(reason) = entry.needs_human.get(code) {
            plan.skipped
                .push(Skipped::new(code, SkipReason::NeedsHuman, reason.clone()));
            continue;
        }
        if exhausted {
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::AttemptsExhausted,
                format!("当日失败次数已达上限 {MAX_ATTEMPTS_PER_DAY}"),
            ));
            continue;
        }
        if !task.spec.is_known() {
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::UnknownCode,
                "目录不认识这个任务码：不猜 kind，也不占接取名额",
            ));
            continue;
        }
        // 已完成 / 已接取：auto 类也不该再排进事件链（那只会白发一串事件）。
        if task.is_complete() || !task.needs_accept() {
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::AlreadyAccepted,
                format!(
                    "已完成（{}/{}，accept_status={}）",
                    task.current,
                    task.target,
                    task.status.as_str()
                ),
            ));
            continue;
        }
        if task.class.is_auto() {
            plan.auto_first.push(task.code.clone());
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::AutoNoAccept,
                "auto 类不需要接取（由事件链点亮）",
            ));
            continue;
        }
        if !buddy_ready {
            plan.skipped.push(Skipped::new(
                code,
                SkipReason::PrerequisiteFirstBuddy,
                "前置未满足：先完成 first_buddy",
            ));
            continue;
        }
        codes.push(task.code.clone());
    }
    plan.batches = chunk(&codes, ACCEPT_CHUNK);
    plan
}

/// 按 `size` 切批（`size` 为 0 时退成 1，避免死循环）。
pub fn chunk(codes: &[String], size: usize) -> Vec<Vec<String>> {
    let size = size.max(1);
    codes.chunks(size).map(<[String]>::to_vec).collect()
}

// ---------------------------------------------------------------------------
// 间隔
// ---------------------------------------------------------------------------

/// 确定性抖动：`-25..=25`（同一 `seed` 永远同一结果）。
///
/// 不引 RNG 仓库、也不读时钟：抖动的目的只是「别让每一轮都卡在同一毫秒网格上」，
/// 不需要密码学强度；确定性换来的好处是边界能在测试里钉死。
pub fn jitter_pct(seed: u64) -> i64 {
    let span = 2 * GAP_JITTER_PCT + 1;
    (seed % span) as i64 - GAP_JITTER_PCT as i64
}

/// 两步之间的间隔：`max(引擎 gap, 风控 min_gap) + jitter%`。
///
/// 两条纪律：
/// - 风控**关着**时不采信它的 `min_account_gap_ms`（5s 只是风控自己的缺省，
///   用户关掉风控却还被 5s 卡着，等于开关没生效）；引擎的 1s 是常量，永远在。
/// - 抖动为负时**不低于 [`ENGINE_GAP_MS`]**（上游按行为计时，见 spec §2.2 #18）。
///   下限取 `min(engine_gap, ENGINE_GAP_MS)`：调用方显式把引擎间隔配得更小时
///   （测试传 0）按它自己的值走，不硬塞一个等不来的 1s。
pub fn step_gap_ms(
    engine_gap_ms: u64,
    risk_enabled: bool,
    risk_min_gap_ms: u64,
    jitter_pct: i64,
) -> u64 {
    let risk_gap = if risk_enabled { risk_min_gap_ms } else { 0 };
    let base = engine_gap_ms.max(risk_gap) as i64;
    let bounded = jitter_pct.clamp(-(GAP_JITTER_PCT as i64), GAP_JITTER_PCT as i64);
    let jittered = base + base * bounded / 100;
    let floor = engine_gap_ms.min(ENGINE_GAP_MS) as i64;
    jittered.max(floor) as u64
}

/// 一轮的配置（`now_ms` 由调用方注入，引擎自己不读时钟）。
#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub now_ms: i64,
    /// 引擎自带间隔，默认 [`ENGINE_GAP_MS`]；测试传 0 关掉等待。
    pub engine_gap_ms: u64,
    /// `risk_control` 的总开关（关着时不采信 `min_account_gap_ms`）。
    pub risk_enabled: bool,
    pub risk_min_gap_ms: u64,
    /// 抖动种子：每个调用方给一个固定值，同一轮内逐步递增。
    pub jitter_seed: u64,
}

impl EngineConfig {
    pub fn new(now_ms: i64) -> Self {
        Self {
            now_ms,
            engine_gap_ms: ENGINE_GAP_MS,
            risk_enabled: false,
            risk_min_gap_ms: 0,
            jitter_seed: 0,
        }
    }

    /// 第 `step` 步之后该等多久。
    pub fn gap_ms(&self, step: u64) -> u64 {
        step_gap_ms(
            self.engine_gap_ms,
            self.risk_enabled,
            self.risk_min_gap_ms,
            jitter_pct(self.jitter_seed.wrapping_add(step)),
        )
    }
}

// ---------------------------------------------------------------------------
// 接取结果
// ---------------------------------------------------------------------------

/// 一批 accept 的结果（逐条可见，**失败不吞** —— 参考实现当年就是因为吞了失败，
/// 才出现「接取了一堆但一个都没点亮」）。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AcceptBatch {
    pub requested: Vec<String>,
    pub accepted: Vec<String>,
    pub failed: Vec<String>,
    pub reasons: Vec<String>,
    /// 整批失败的原因（`code != 0` / 请求没发出）。
    pub error: Option<String>,
}

impl AcceptBatch {
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.failed.is_empty()
    }
}

/// 解析 accept 响应：`data.results[{task_code, status, message}]`。
///
/// 三种失败都要留痕：`status` 不是 `accepted`/`already_accepted`、`code != 0`、
/// **请求里发了但响应里没提**（参考实现同样把「没出现的码」计入 failed）。
pub fn parse_accept(body: &Value, requested: &[String]) -> AcceptBatch {
    let mut batch = AcceptBatch {
        requested: requested.to_vec(),
        ..Default::default()
    };
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        batch.error = Some(reply_error("accept", body));
        batch.failed = requested.to_vec();
        batch.reasons.push(batch.error.clone().unwrap_or_default());
        return batch;
    }
    let results = body
        .pointer("/data/results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut seen: Vec<String> = Vec::new();
    for item in results {
        let task_code = item
            .get("task_code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if task_code.is_empty() {
            continue;
        }
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let message = item.get("message").and_then(Value::as_str).unwrap_or("");
        seen.push(task_code.clone());
        if status == "accepted" || status == "already_accepted" {
            batch.accepted.push(task_code);
        } else {
            batch
                .reasons
                .push(sanitize_error(&format!("{task_code}: {message}{status}")));
            batch.failed.push(task_code);
        }
    }
    for code in requested {
        if !seen.contains(code) {
            batch.reasons.push(format!("{code}: missing from results"));
            batch.failed.push(code.clone());
        }
    }
    batch
}

/// 上游错误拼成一句**脱敏**的话。
fn reply_error(action: &str, body: &Value) -> String {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let message = body
        .get("message")
        .or_else(|| body.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("");
    sanitize_error(&format!("{action}: code={code} {message}"))
}

// ---------------------------------------------------------------------------
// 出网（读 / 写都走 TaskIo）
// ---------------------------------------------------------------------------

/// 成长任务用的附加头：chat 形态（与参考实现的 `account.headers("chat")` 同义）。
///
/// 取不到（客户端版本缺失）就退回空表：头只决定「像哪个客户端」，不该让整轮任务停摆。
fn growth_headers(region: Region) -> HashMap<String, String> {
    let base = region_spec(region).chat_base;
    outbound_headers::chat_for_region(region, base)
        .map(|set| set.to_map())
        .unwrap_or_default()
}

/// 读清单。`status == 0` 表示**请求没发出**（风控准入拦截 / 凭据缺失），直接当错误返回，
/// 让调用方当轮停手（spec §3.2：被拒即当轮停止该账号，不换号接力）。
pub async fn fetch_growth(
    io: &dyn TaskIo,
    account: &Value,
    region: Region,
) -> Result<Vec<GrowthTask>, String> {
    let url = format!("{}{TASKS_PATH}", region_spec(region).chat_base);
    let call = TaskCall::new(region, &url, account).headers(growth_headers(region));
    let (status, body) = io.get(call).await;
    if status == 0 {
        return Err(format!(
            "growth tasks fetch not sent: {}",
            reply_error("fetch", &body)
        ));
    }
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        return Err(reply_error("growth tasks fetch", &body));
    }
    Ok(parse_list(&body))
}

/// 一轮接取的结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrowthRound {
    /// 闸门结论（非 `proceed` 时本轮什么都没发）。
    pub gate: Option<String>,
    pub plan: AcceptPlan,
    pub batches: Vec<AcceptBatch>,
    /// 上游确认接上的码（含 `already_accepted`）。
    pub accepted: Vec<String>,
    /// 上游拒绝 / 没回音的码。
    pub failed: Vec<String>,
    /// 读清单次数（含复读）。
    pub fetched: u32,
    /// 其中复读的次数。
    pub refetched: u32,
    /// 是否发生了「再试一次」。
    pub retried: bool,
    /// 实际发出的写请求数（= 多少个 accept 批次）。
    pub actions: u32,
    /// 本轮最后的错误（脱敏）。出现即说明有一批没好，但**不等于**整轮失败。
    pub error: Option<String>,
    /// 复读后的清单，交给下一步（上报 / 领奖）用，避免再读一次。
    pub final_tasks: Vec<GrowthTask>,
}

impl GrowthRound {
    /// 没有失败、也没有错误。
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.failed.is_empty()
    }

    /// 上游确认接上的码里是否包含 `code`。
    pub fn accepted_contains(&self, code: &str) -> bool {
        self.accepted.iter().any(|item| item == code)
    }
}

/// 跑一轮「读清单 → 接取 → 复读 → 重试一次」。
///
/// 闸门（企业号 / 非 CN）在**发第一个请求之前**判定，返回 [`GrowthRound::gate`]。
pub async fn run_accept_round(
    io: &dyn TaskIo,
    ledger: &TaskLedger,
    account: &Value,
    region: Region,
    cfg: &EngineConfig,
) -> Result<GrowthRound, String> {
    let mut round = GrowthRound::default();
    let gate = task_catalog::gate_for(region, account);
    if gate != Gate::Proceed {
        round.gate = Some(gate.as_str().to_string());
        return Ok(round);
    }
    let uid = get_str(account, "uid").ok_or_else(|| "account has no uid".to_string())?;
    let key = TaskDayKey::for_now(
        WORKBUDDY_PRODUCT,
        region.as_str(),
        &uid,
        GROWTH_FEATURE,
        cfg.now_ms,
    )?;

    let mut tasks = fetch_growth(io, account, region).await?;
    round.fetched = 1;
    let entry = ledger.entry(&key)?;
    let plan = plan_accept(&tasks, &entry, cfg.now_ms);
    round.plan = plan.clone();
    // 「需真人」是中性状态，先落台账：下一轮连清单都不用重新判定。
    for item in &plan.needs_human {
        ledger.mark_needs_human(&key, &item.code, &item.detail, cfg.now_ms)?;
    }
    if plan.batches.is_empty() {
        round.final_tasks = tasks;
        return Ok(round);
    }

    let mut step = 0u64;
    let first_codes: Vec<String> = plan.codes().into_iter().map(str::to_string).collect();
    run_batches(
        io, ledger, &key, account, region, cfg, &plan.batches, &mut round, &mut step,
    )
    .await?;

    // 复读：确认「谁真的接上了」，顺便抓到这期间新出现 / 新完成的任务。
    pause(cfg, &mut step).await;
    tasks = fetch_growth(io, account, region).await?;
    round.fetched += 1;
    round.refetched += 1;
    round.final_tasks = tasks.clone();

    // 只重试一次：第一轮喊要接、但上游没确认接上的码，且复读后仍然「未接取」。
    let entry = ledger.entry(&key)?;
    let replan = plan_accept(&tasks, &entry, cfg.now_ms);
    let still: Vec<String> = replan
        .codes()
        .into_iter()
        .filter(|code| first_codes.iter().any(|item| item == code))
        .map(str::to_string)
        .collect();
    // 首轮已用掉 `MAX_ACCEPT_ROUNDS` 的一半，这里只放一次重试。
    if still.is_empty() || MAX_ACCEPT_ROUNDS <= 1 {
        return Ok(round);
    }
    round.retried = true;
    let retry_batches = chunk(&still, ACCEPT_CHUNK);
    run_batches(
        io,
        ledger,
        &key,
        account,
        region,
        cfg,
        &retry_batches,
        &mut round,
        &mut step,
    )
    .await?;

    pause(cfg, &mut step).await;
    round.final_tasks = fetch_growth(io, account, region).await?;
    round.fetched += 1;
    round.refetched += 1;
    Ok(round)
}

async fn run_batches(
    io: &dyn TaskIo,
    ledger: &TaskLedger,
    key: &TaskDayKey,
    account: &Value,
    region: Region,
    cfg: &EngineConfig,
    batches: &[Vec<String>],
    round: &mut GrowthRound,
    step: &mut u64,
) -> Result<(), String> {
    let url = format!("{}{ACCEPT_PATH}", region_spec(region).chat_base);
    for batch in batches {
        // 先记账再动手：请求发出后进程崩了，台账里也已经留下「花了一个名额」。
        ledger.record_action(key, cfg.now_ms)?;
        round.actions = round.actions.saturating_add(1);
        let call = TaskCall::new(region, &url, account).headers(growth_headers(region));
        let (status, body) = io.post(call, json!({ "task_codes": batch })).await;
        let parsed = if status == 0 {
            let mut blocked = AcceptBatch {
                requested: batch.clone(),
                failed: batch.clone(),
                error: Some(reply_error("accept", &body)),
                ..Default::default()
            };
            blocked.reasons.push("风控准入拦截：请求没有发出".to_string());
            blocked
        } else {
            parse_accept(&body, batch)
        };
        for code in &parsed.accepted {
            if !round.accepted.iter().any(|item| item == code) {
                round.accepted.push(code.clone());
            }
        }
        for code in &parsed.failed {
            if !round.failed.iter().any(|item| item == code) {
                round.failed.push(code.clone());
            }
        }
        if let Some(error) = &parsed.error {
            round.error = Some(error.clone());
            // 整批失败才算失败：单条被拒不推进 attempts，否则「还没到」的状态会烧掉当日预算。
            ledger.record_failure(key, error, cfg.now_ms)?;
        }
        round.batches.push(parsed);
        pause(cfg, step).await;
    }
    Ok(())
}

async fn pause(cfg: &EngineConfig, step: &mut u64) {
    *step = step.saturating_add(1);
    let gap = cfg.gap_ms(*step);
    if gap > 0 {
        tokio::time::sleep(Duration::from_millis(gap)).await;
    }
}
