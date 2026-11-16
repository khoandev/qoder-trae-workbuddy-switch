//! WorkBuddy 任务自动化的**每日台账**（防重复上报 / 防重复领奖 / 防同日重跑）。
//!
//! 参考实现 `wb2api@22b7966` 的 `wb_tasks.py` 没有台账：它靠上游 `accept_status` 与
//! 「本轮进程里记住刚才报过什么」来判断，进程一重启就全忘。迁进本仓后，调度器一天会
//! 唤醒多次（还有用户手动「立即执行」），没有台账就会出现两件坏事：
//!
//! - 同一任务当天被**反复上报**（上游按 `(eventCode, id)` 去重，重复只增加请求密度）；
//! - 同一奖励当天被**反复领奖**（`400 task not completed` 只是噪音，真领到过再打就是浪费）。
//!
//! 所以这里落一张**按天**的台账：键固定为
//! `{product}/{region}/{account_id}/{feature}/{YYYY-MM-DD}`（见 [`TaskDayKey`]），
//! 值记 `attempts` / `actions` / `claimed` / `needs_human` / `last_error`。
//!
//! ## 三条硬规则（改之前先读）
//!
//! 1. **fail-closed**：台账文件损坏时 [`TaskLedger::open`] 返回 `Err`，**不要**当成空台账继续跑。
//!    今天的账丢了，重跑一次等于重复领奖；宁可今天不做，明天重来。
//! 2. **写失败不推进**：每次修改都是「重新读 → 改内存 → [`atomic_write`] 落盘」。
//!    落盘失败即返回 `Err`，调用方必须当本轮失败处理（内存副本直接丢弃，不会半写）。
//! 3. **错误串必须脱敏**：`last_error` 会进账号页，也会进诊断包。落盘前一律过
//!    [`crate::modules::runtime_log::redact`]，并截断到 [`MAX_ERROR_CHARS`]。
//!
//! 保留窗口 [`RETENTION_DAYS`]：任务判据都是「当日」口径，30 天足够复盘，也不让文件长胖。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::NaiveDate;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::cst::cst_date;
use crate::modules::runtime_log::redact;

/// 台账保留天数（按台账里的 `date` 字段算，不是按 `updated_at`）。
pub const RETENTION_DAYS: i64 = 30;
/// 同一 `(账号, feature, 任务)` 当日允许的失败次数上限；到顶就当天停手。
pub const MAX_ATTEMPTS_PER_DAY: u32 = 3;
/// 条数安全网：日期解析失败的条目不会被日期裁剪掉，靠这条兜底。
pub const MAX_ENTRIES: usize = 2_000;
/// `last_error` 落盘前的最大字符数（按 `char` 截断，不会切坏 UTF-8）。
pub const MAX_ERROR_CHARS: usize = 300;
/// 台账文件名（与 `risk_control.json` 同目录）。
pub const LEDGER_FILE: &str = "wb_automation_ledger.json";

const SCHEMA_VERSION: u32 = 1;

/// 台账的「一天一格」键：`{product}/{region}/{account_id}/{feature}/{date}`。
///
/// 五个分量都是**必有**的：少了 `feature` 会让「成长任务」与「运营活动」共享一次
/// `attempts` 预算（一边失败把另一边也停掉）；少了 `date` 就变成「永久台账」，
/// 领过的奖励再也不会重做。这两条都是**静默**的行为错误，所以键在这里收口。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskDayKey {
    pub product: String,
    pub region: String,
    pub account_id: String,
    pub feature: String,
    pub date: NaiveDate,
}

impl TaskDayKey {
    /// 显式日期构造（测试与「补昨天」这类场景用）。
    pub fn new(
        product: &str,
        region: &str,
        account_id: &str,
        feature: &str,
        date: NaiveDate,
    ) -> Result<Self, String> {
        let product = product.trim().to_ascii_lowercase();
        let region = region.trim().to_ascii_lowercase();
        check_slug("product", &product)?;
        check_slug("region", &region)?;
        check_component("account_id", account_id)?;
        check_component("feature", feature)?;
        Ok(Self {
            product,
            region,
            account_id: account_id.to_string(),
            feature: feature.to_string(),
            date,
        })
    }

    /// 以 **CST 日期**构造当天的键（与任务链的「当日」口径一致，见 `modules::cst`）。
    pub fn for_now(
        product: &str,
        region: &str,
        account_id: &str,
        feature: &str,
        now_ms: i64,
    ) -> Result<Self, String> {
        Self::new(product, region, account_id, feature, cst_date(now_ms))
    }

    /// 落盘用的扁平键（也是 `entries` 的 map key）。
    pub fn as_storage_key(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            self.product,
            self.region,
            self.account_id,
            self.feature,
            self.date.format("%Y-%m-%d")
        )
    }

    /// 诊断 / 日志用标签。**不含账号 UID 之外的凭据**，也刻意不带时间戳，
    /// 这样同一格在日志里能聚合。
    pub fn label(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.product, self.region, self.account_id, self.feature
        )
    }
}

fn check_slug(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 64 {
        return Err(format!("{field} must contain 1..=64 characters"));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!(
            "{field} may contain only ASCII letters, digits, '-' and '_'"
        ));
    }
    Ok(())
}

fn check_component(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.chars().count() > 128 {
        return Err(format!("{field} must contain 1..=128 characters"));
    }
    // `/` 是键分隔符：允许它会造出两个不同逻辑格映射到同一个字符串。
    if value.contains('/') || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("{field} must not contain '/', whitespace or control chars"));
    }
    Ok(())
}

/// 当天某任务已经领到的奖励（`credit` 用于账号页「今天赚了多少」）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedTask {
    #[serde(default)]
    pub credit: i64,
    #[serde(default)]
    pub at_ms: i64,
}

/// 一天的台账格。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskDayEntry {
    /// 失败次数（**不含**「还没完成」这类正常中间态）。
    pub attempts: u32,
    /// 实际发出的写请求数（accept / report / claim），只用于诊断。
    pub actions: u32,
    /// 任务码 → 领奖记录。
    pub claimed: BTreeMap<String, ClaimedTask>,
    /// 任务码 → 需要真人操作的原因。
    pub needs_human: BTreeMap<String, String>,
    /// 最近一次失败的可读原因（已脱敏）。
    pub last_error: String,
    pub updated_at: i64,
}

impl TaskDayEntry {
    /// 当日累计领到的积分。
    pub fn credit_total(&self) -> i64 {
        self.claimed.values().map(|c| c.credit).sum()
    }
}

/// [`TaskLedger::guard`] 的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskGuard {
    /// 今天还没做过这个任务，可以推进。
    Proceed,
    /// 今天已经领过奖 —— 不再上报、不再领。
    AlreadyClaimed { credit: i64 },
    /// 今天已判定需要真人操作 —— 当轮跳过，**不算失败**。
    NeedsHuman { reason: String },
    /// 今天的失败次数到顶 —— 当天停手，明天重来。
    AttemptsExhausted { attempts: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct LedgerFile {
    version: u32,
    entries: BTreeMap<String, TaskDayEntry>,
}

impl Default for LedgerFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// 进程内串行化「读 → 改 → 写」，避免两个调度轮次互相覆盖（文件锁留给 OS，
/// 这里只需要管住同一进程里的并发写）。
static LEDGER_LOCK: Mutex<()> = Mutex::new(());

/// 按天台账。**无状态句柄**：每次操作都重新读文件，所以多处 `open` 同一路径是安全的。
pub struct TaskLedger {
    path: PathBuf,
}

impl TaskLedger {
    /// 打开指定路径的台账。文件不存在 ⇒ 空台账；文件损坏 ⇒ **`Err`（fail-closed）**。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let ledger = Self {
            path: path.as_ref().to_path_buf(),
        };
        // 先读一次：把「损坏」和「不存在」在这里分开，别等第一次写才发现。
        ledger.load_map()?;
        Ok(ledger)
    }

    /// 应用数据目录下的默认台账。
    pub fn in_store() -> Result<Self, String> {
        Self::open(store_dir().join(LEDGER_FILE))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读整张台账（键是 [`TaskDayKey::as_storage_key`]）。
    pub fn entries(&self) -> Result<BTreeMap<String, TaskDayEntry>, String> {
        self.load_map()
    }

    /// 读某一格（缺失 ⇒ 默认值）。
    pub fn entry(&self, key: &TaskDayKey) -> Result<TaskDayEntry, String> {
        Ok(self
            .load_map()?
            .remove(&key.as_storage_key())
            .unwrap_or_default())
    }

    /// 某账号今天的全部 feature 台账（账号页展示「今天做了什么」）。
    pub fn day_entries(
        &self,
        product: &str,
        region: &str,
        account_id: &str,
        now_ms: i64,
    ) -> Result<BTreeMap<String, TaskDayEntry>, String> {
        let prefix = format!(
            "{}/{}/{}/",
            product.trim().to_ascii_lowercase(),
            region.trim().to_ascii_lowercase(),
            account_id
        );
        let today = cst_date(now_ms).format("%Y-%m-%d").to_string();
        let mut out = BTreeMap::new();
        for (storage_key, entry) in self.load_map()? {
            let Some(rest) = storage_key.strip_prefix(&prefix) else {
                continue;
            };
            let Some((feature, date)) = rest.rsplit_once('/') else {
                continue;
            };
            if date == today {
                out.insert(feature.to_string(), entry);
            }
        }
        Ok(out)
    }

    /// 幂等守卫：这个任务今天能不能继续做。
    ///
    /// 判据顺序（**顺序本身是语义**）：
    /// 1. 已领奖 ⇒ `AlreadyClaimed`（哪怕此前失败过几次，领到了就是领到了）；
    /// 2. 已判 needs_human ⇒ `NeedsHuman`（不是故障，不要记 `attempts`）；
    /// 3. 失败次数到顶 ⇒ `AttemptsExhausted`；
    /// 4. 否则 `Proceed`。
    pub fn guard(&self, key: &TaskDayKey, task_code: &str) -> Result<TaskGuard, String> {
        let entry = self.entry(key)?;
        if let Some(claimed) = entry.claimed.get(task_code) {
            return Ok(TaskGuard::AlreadyClaimed {
                credit: claimed.credit,
            });
        }
        if let Some(reason) = entry.needs_human.get(task_code) {
            return Ok(TaskGuard::NeedsHuman {
                reason: reason.clone(),
            });
        }
        if entry.attempts >= MAX_ATTEMPTS_PER_DAY {
            return Ok(TaskGuard::AttemptsExhausted {
                attempts: entry.attempts,
            });
        }
        Ok(TaskGuard::Proceed)
    }

    /// 记一次实际发出的写请求。
    pub fn record_action(&self, key: &TaskDayKey, at_ms: i64) -> Result<TaskDayEntry, String> {
        self.mutate(key, at_ms, |entry| {
            entry.actions = entry.actions.saturating_add(1);
        })
    }

    /// 记一次失败：`attempts += 1` + 脱敏后的 `last_error`。
    ///
    /// ⚠️ **「任务还没完成」不算失败**（上游 `400 task not completed` 只是「还没到」），
    /// 调用方别把它送进来，否则真的任务会被三次空转停掉一整天。
    pub fn record_failure(
        &self,
        key: &TaskDayKey,
        message: &str,
        at_ms: i64,
    ) -> Result<TaskDayEntry, String> {
        let sanitized = sanitize_error(message);
        self.mutate(key, at_ms, |entry| {
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_error = sanitized.clone();
        })
    }

    /// 记一次领奖成功（此后当天不再上报/领奖）。
    pub fn mark_claimed(
        &self,
        key: &TaskDayKey,
        task_code: &str,
        credit: i64,
        at_ms: i64,
    ) -> Result<TaskDayEntry, String> {
        self.mutate(key, at_ms, |entry| {
            entry.claimed.insert(
                task_code.to_string(),
                ClaimedTask {
                    credit,
                    at_ms,
                },
            );
            // 领到了就不再是故障态，也别让旧错误继续挂在账号页。
            entry.last_error.clear();
        })
    }

    /// 记一次「需要真人操作」（中性状态，不是失败）。
    pub fn mark_needs_human(
        &self,
        key: &TaskDayKey,
        task_code: &str,
        reason: &str,
        at_ms: i64,
    ) -> Result<TaskDayEntry, String> {
        let reason = sanitize_error(reason);
        self.mutate(key, at_ms, |entry| {
            entry.needs_human.insert(task_code.to_string(), reason.clone());
        })
    }

    /// 清掉某任务当天的全部痕迹（用户手动「重试今天」时用）。
    pub fn forget_task(&self, key: &TaskDayKey, task_code: &str) -> Result<TaskDayEntry, String> {
        self.mutate(key, now_ms(), |entry| {
            entry.claimed.remove(task_code);
            entry.needs_human.remove(task_code);
            entry.last_error.clear();
        })
    }

    /// 按 [`RETENTION_DAYS`] 裁剪。返回删掉的格数。
    ///
    /// 日期解析失败的条目**不按日期删**（那会把别人的数据当垃圾），只在总数超过
    /// [`MAX_ENTRIES`] 时按 `updated_at` 丢掉最旧的。
    pub fn prune(&self, now_ms: i64) -> Result<usize, String> {
        let today = cst_date(now_ms);
        let cutoff = today - chrono::Duration::days(RETENTION_DAYS);
        let _guard = LEDGER_LOCK
            .lock()
            .map_err(|_| "task ledger lock poisoned".to_string())?;
        let mut file = self.read_file()?;
        let before = file.entries.len();
        file.entries.retain(|storage_key, _| {
            let Some(date) = storage_key.rsplit('/').next() else {
                return true;
            };
            match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
                Ok(date) => date >= cutoff,
                Err(_) => true,
            }
        });
        if file.entries.len() > MAX_ENTRIES {
            let mut by_age: Vec<(String, i64)> = file
                .entries
                .iter()
                .map(|(key, entry)| (key.clone(), entry.updated_at))
                .collect();
            by_age.sort_by_key(|(_, updated_at)| *updated_at);
            let excess = file.entries.len() - MAX_ENTRIES;
            for (key, _) in by_age.into_iter().take(excess) {
                file.entries.remove(&key);
            }
        }
        let removed = before.saturating_sub(file.entries.len());
        self.write_file(&file)?;
        Ok(removed)
    }

    fn mutate<F>(&self, key: &TaskDayKey, at_ms: i64, change: F) -> Result<TaskDayEntry, String>
    where
        F: FnOnce(&mut TaskDayEntry),
    {
        let _guard = LEDGER_LOCK
            .lock()
            .map_err(|_| "task ledger lock poisoned".to_string())?;
        let mut file = self.read_file()?;
        let storage_key = key.as_storage_key();
        let entry = file.entries.entry(storage_key).or_default();
        change(entry);
        entry.updated_at = at_ms;
        let snapshot = entry.clone();
        self.write_file(&file)?;
        Ok(snapshot)
    }

    fn load_map(&self) -> Result<BTreeMap<String, TaskDayEntry>, String> {
        let _guard = LEDGER_LOCK
            .lock()
            .map_err(|_| "task ledger lock poisoned".to_string())?;
        Ok(self.read_file()?.entries)
    }

    fn read_file(&self) -> Result<LedgerFile, String> {
        if !self.path.exists() {
            return Ok(LedgerFile::default());
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| format!("read task ledger: {error}"))?;
        if text.trim().is_empty() {
            return Ok(LedgerFile::default());
        }
        let file: LedgerFile = serde_json::from_str(&text)
            .map_err(|error| format!("parse task ledger: {error}"))?;
        if file.version == 0 || file.version > SCHEMA_VERSION {
            // 未来版本的文件**不要**按当前 schema 解读后写回：那会把新字段抹掉。
            return Err(format!(
                "task ledger schema version {} is not supported (expected {SCHEMA_VERSION})",
                file.version
            ));
        }
        Ok(file)
    }

    fn write_file(&self, file: &LedgerFile) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create task ledger dir: {error}"))?;
        }
        let text = serde_json::to_string_pretty(file)
            .map_err(|error| format!("serialize task ledger: {error}"))?;
        atomic_write(&self.path, &text).map_err(|error| format!("write task ledger: {error}"))
    }
}

/// 失败原因脱敏 + 截断（`last_error` 会进账号页与诊断包）。
pub fn sanitize_error(message: &str) -> String {
    let redacted = redact(message);
    let mut out: String = redacted.chars().take(MAX_ERROR_CHARS).collect();
    if redacted.chars().count() > MAX_ERROR_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::cst::cst_date_str;

    // 2026-11-16 08:00 CST（固定值：让「当天 / 次日 / 30 天前」三条断言与跑测试的日期无关）。
    const NOW: i64 = 1_794_787_200_000;

    fn ledger() -> (tempfile::TempDir, TaskLedger) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = TaskLedger::open(dir.path().join(LEDGER_FILE)).expect("open ledger");
        (dir, ledger)
    }

    fn key(account: &str, feature: &str) -> TaskDayKey {
        TaskDayKey::for_now("workbuddy", "cn", account, feature, NOW).expect("key")
    }

    #[test]
    fn key_is_stable_and_rejects_ambiguous_components() {
        let k = key("uid-1", "growth_tasks");
        assert_eq!(
            k.as_storage_key(),
            format!("workbuddy/cn/uid-1/growth_tasks/{}", cst_date_str(NOW))
        );
        assert_eq!(k.label(), "workbuddy/cn/uid-1/growth_tasks");

        // `/` 会让两个逻辑格映射到同一个键；空白与空段同理。
        assert!(TaskDayKey::new("workbuddy", "cn", "a/b", "growth", cst_date(NOW)).is_err());
        assert!(TaskDayKey::new("workbuddy", "cn", "uid", "growth tasks", cst_date(NOW)).is_err());
        assert!(TaskDayKey::new("workbuddy", "cn", "", "growth", cst_date(NOW)).is_err());
        assert!(TaskDayKey::new("work buddy", "cn", "uid", "growth", cst_date(NOW)).is_err());
    }

    #[test]
    fn fresh_day_proceeds_and_claimed_blocks_repeat_claim() {
        let (_dir, ledger) = ledger();
        let k = key("uid-1", "growth_tasks");
        assert_eq!(ledger.guard(&k, "chat_5").unwrap(), TaskGuard::Proceed);

        ledger.mark_claimed(&k, "chat_5", 100, NOW).unwrap();
        assert_eq!(
            ledger.guard(&k, "chat_5").unwrap(),
            TaskGuard::AlreadyClaimed { credit: 100 }
        );
        // 领奖只挡同一个任务码，不挡同账号的其它任务。
        assert_eq!(ledger.guard(&k, "expert_5").unwrap(), TaskGuard::Proceed);

        let entry = ledger.entry(&k).unwrap();
        assert_eq!(entry.credit_total(), 100);
    }

    #[test]
    fn needs_human_is_not_a_failure_and_blocks_retry() {
        let (_dir, ledger) = ledger();
        let k = key("uid-1", "growth_tasks");
        ledger
            .mark_needs_human(&k, "wb_wechat_oa_subscribe_task", "关注公众号满 24h", NOW)
            .unwrap();
        for _ in 0..(MAX_ATTEMPTS_PER_DAY + 2) {
            assert!(matches!(
                ledger.guard(&k, "wb_wechat_oa_subscribe_task").unwrap(),
                TaskGuard::NeedsHuman { .. }
            ));
        }
        assert_eq!(ledger.entry(&k).unwrap().attempts, 0);
    }

    #[test]
    fn attempts_are_capped_per_day_and_reset_on_the_next_day() {
        let (_dir, ledger) = ledger();
        let k = key("uid-1", "growth_tasks");
        for _ in 0..MAX_ATTEMPTS_PER_DAY {
            ledger.record_failure(&k, "HTTP 500", NOW).unwrap();
        }
        assert_eq!(
            ledger.guard(&k, "chat_5").unwrap(),
            TaskGuard::AttemptsExhausted {
                attempts: MAX_ATTEMPTS_PER_DAY
            }
        );

        // 次日是新的一格，预算重新开始。
        let tomorrow =
            TaskDayKey::for_now("workbuddy", "cn", "uid-1", "growth_tasks", NOW + 86_400_000)
                .unwrap();
        assert_eq!(ledger.guard(&tomorrow, "chat_5").unwrap(), TaskGuard::Proceed);
    }

    #[test]
    fn features_and_accounts_do_not_share_the_attempt_budget() {
        let (_dir, ledger) = ledger();
        let growth = key("uid-1", "growth_tasks");
        let campaign = key("uid-1", "campaign");
        let other = key("uid-2", "growth_tasks");
        for _ in 0..MAX_ATTEMPTS_PER_DAY {
            ledger.record_failure(&growth, "boom", NOW).unwrap();
        }
        assert!(matches!(
            ledger.guard(&growth, "chat_5").unwrap(),
            TaskGuard::AttemptsExhausted { .. }
        ));
        assert_eq!(ledger.guard(&campaign, "chat_5").unwrap(), TaskGuard::Proceed);
        assert_eq!(ledger.guard(&other, "chat_5").unwrap(), TaskGuard::Proceed);
    }

    #[test]
    fn entries_survive_reopen_and_stay_readable_for_the_account_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LEDGER_FILE);
        {
            let ledger = TaskLedger::open(&path).unwrap();
            ledger
                .mark_claimed(&key("uid-1", "growth_tasks"), "chat_5", 100, NOW)
                .unwrap();
            ledger
                .mark_claimed(&key("uid-1", "buddy_box"), "buddy_open", 0, NOW)
                .unwrap();
        }
        let reopened = TaskLedger::open(&path).unwrap();
        let day = reopened
            .day_entries("workbuddy", "cn", "uid-1", NOW)
            .unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day["growth_tasks"].credit_total(), 100);
        assert!(day.contains_key("buddy_box"));
        // 别的账号拿到的是空表，不会串账。
        assert!(reopened
            .day_entries("workbuddy", "cn", "uid-2", NOW)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn actions_are_counted_separately_from_failures() {
        let (_dir, ledger) = ledger();
        let k = key("uid-1", "growth_tasks");
        ledger.record_action(&k, NOW).unwrap();
        ledger.record_action(&k, NOW).unwrap();
        ledger.record_failure(&k, "HTTP 500", NOW).unwrap();
        let entry = ledger.entry(&k).unwrap();
        assert_eq!(entry.actions, 2);
        assert_eq!(entry.attempts, 1);
    }

    #[test]
    fn corrupt_or_future_ledger_fails_closed_instead_of_starting_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corrupted = dir.path().join("corrupt.json");
        std::fs::write(&corrupted, "{not json").unwrap();
        assert!(TaskLedger::open(&corrupted).is_err());

        let future = dir.path().join("future.json");
        std::fs::write(&future, r#"{"version":99,"entries":{}}"#).unwrap();
        assert!(TaskLedger::open(&future).is_err());
    }

    #[test]
    fn empty_file_is_treated_as_an_empty_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "  \n").unwrap();
        let ledger = TaskLedger::open(&path).unwrap();
        assert!(ledger.entries().unwrap().is_empty());
    }

    #[test]
    fn prune_drops_old_days_but_keeps_today() {
        let (_dir, ledger) = ledger();
        let today = key("uid-1", "growth_tasks");
        let old_date = cst_date(NOW) - chrono::Duration::days(RETENTION_DAYS + 1);
        let old = TaskDayKey::new("workbuddy", "cn", "uid-1", "growth_tasks", old_date).unwrap();
        ledger.mark_claimed(&old, "chat_5", 100, NOW).unwrap();
        ledger.mark_claimed(&today, "expert_5", 200, NOW).unwrap();

        assert_eq!(ledger.prune(NOW).unwrap(), 1);
        let remaining = ledger.entries().unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(remaining.contains_key(&today.as_storage_key()));
    }

    #[test]
    fn last_error_is_redacted_and_truncated() {
        let (_dir, ledger) = ledger();
        let k = key("uid-1", "growth_tasks");
        ledger
            .record_failure(
                &k,
                "POST failed: Authorization: Bearer sk-live-abcdef123456 &access_token=supersecret",
                NOW,
            )
            .unwrap();
        let entry = ledger.entry(&k).unwrap();
        assert!(!entry.last_error.contains("sk-live-abcdef123456"));
        assert!(!entry.last_error.contains("supersecret"));
        assert!(entry.last_error.contains(crate::modules::runtime_log::REDACTED));

        let long = "x".repeat(MAX_ERROR_CHARS * 2);
        assert_eq!(sanitize_error(&long).chars().count(), MAX_ERROR_CHARS + 1);
    }

    #[test]
    fn forgetting_one_task_keeps_the_others() {
        let (_dir, ledger) = ledger();
        let k = key("uid-1", "growth_tasks");
        ledger.mark_claimed(&k, "chat_5", 100, NOW).unwrap();
        ledger.mark_claimed(&k, "expert_5", 200, NOW).unwrap();
        ledger.forget_task(&k, "chat_5").unwrap();
        let entry = ledger.entry(&k).unwrap();
        assert!(!entry.claimed.contains_key("chat_5"));
        assert!(entry.claimed.contains_key("expert_5"));
        assert_eq!(ledger.guard(&k, "chat_5").unwrap(), TaskGuard::Proceed);
    }
}
