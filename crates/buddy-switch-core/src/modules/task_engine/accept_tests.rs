//! 成长引擎的离线用例（`crate::modules::task_engine`）。
//!
//! 这一组整体挂在 `modules::task_engine_accept` 下，一条命令只跑它：
//! `cargo test -p buddy-switch-core --lib modules::task_engine_accept`。
//!
//! 两条纪律：
//! 1. **不发真请求**：出网全部经 `task_io::FakeTaskIo`（它只在测试构建里存在）。
//! 2. **钉住判据，不钉实现细节**：断言的是「接哪些码、分几批、谁被跳过、台账记了什么」，
//!    这些是 spec §3.6 承诺的行为；具体调用顺序只看「读/写各几次」。

use serde_json::{json, Value};

use crate::modules::region::{region_spec, Region};
use crate::modules::task_catalog::{self, GROWTH_TASKS};
use crate::modules::task_engine::{
    chunk, first_buddy_ready, jitter_pct, order_for_run, parse_accept, parse_list, plan_accept,
    run_accept_round, step_gap_ms, AcceptStatus, EngineConfig, GrowthTask, SkipReason, TaskClass,
    ACCEPT_CHUNK, ACCEPT_PATH, ENGINE_GAP_MS, FIRST_BUDDY, GROWTH_FEATURE, TASKS_PATH,
};
use crate::modules::task_io::{FakeTaskIo, WORKBUDDY_PRODUCT};
use crate::modules::task_ledger::{ClaimedTask, TaskDayEntry, TaskDayKey, TaskLedger, LEDGER_FILE};

/// 2026-11-16 08:00 CST（夜间窗口**外**）。
const NOW: i64 = 1_794_787_200_000;
/// 同一天的 23:00 CST（夜间窗口**内**）。
const NIGHT: i64 = NOW + 15 * 3_600_000;

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn raw(code: &str, class: &str, status: &str, current: i64) -> Value {
    json!({
        "task_code": code,
        "title": "",
        "accept_status": status,
        "task_type": class,
        "progress": {"current": current, "target": 1},
    })
}

fn list_body(tasks: Vec<Value>) -> Value {
    json!({"code": 0, "msg": "ok", "data": {"tasks": tasks}})
}

fn parse(tasks: Vec<Value>) -> Vec<GrowthTask> {
    parse_list(&list_body(tasks))
}

fn accept_body(results: Vec<Value>) -> Value {
    json!({"code": 0, "data": {"results": results}})
}

fn accepted(code: &str) -> Value {
    json!({"task_code": code, "status": "accepted"})
}

fn cfg() -> EngineConfig {
    EngineConfig {
        now_ms: NOW,
        engine_gap_ms: 0,
        risk_enabled: false,
        risk_min_gap_ms: 0,
        jitter_seed: 0,
    }
}

fn account() -> Value {
    json!({"uid": "uid-1", "access_token": "tok-secret", "nickname": "tester"})
}

fn ledger() -> (tempfile::TempDir, TaskLedger) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = TaskLedger::open(dir.path().join(LEDGER_FILE)).expect("open ledger");
    (dir, ledger)
}

fn key() -> TaskDayKey {
    TaskDayKey::for_now(WORKBUDDY_PRODUCT, "cn", "uid-1", GROWTH_FEATURE, NOW).expect("key")
}

fn base() -> &'static str {
    region_spec(Region::Cn).chat_base
}

fn reasons(plan: &crate::modules::task_engine::AcceptPlan) -> Vec<(&str, SkipReason)> {
    plan.skipped
        .iter()
        .map(|item| (item.code.as_str(), item.reason))
        .collect()
}

// ---------------------------------------------------------------------------
// 清单解析
// ---------------------------------------------------------------------------

#[test]
fn parse_list_takes_server_fields_over_the_catalog() {
    let body = json!({"code": 0, "data": {"tasks": [
        {
            "task_code": "chat_5",
            "title": "服务端标题",
            "task_desc": "服务端说明",
            "jump_url": "https://www.workbuddy.cn/growth/chat",
            "accept_status": "accepted",
            "task_type": "single",
            "progress": {"current": 3, "target": 9},
            "reward_credit": 250,
            "reward_energy": 7,
        },
        {"task_code": "create_canvas", "task_type": "single"},
    ]}});
    let parsed = parse_list(&body);
    assert_eq!(parsed.len(), 2);

    let chat = &parsed[0];
    assert_eq!(chat.code, "chat_5");
    assert_eq!(chat.spec.name, "服务端标题");
    assert_eq!(chat.description, "服务端说明");
    assert_eq!(chat.jump_url, "https://www.workbuddy.cn/growth/chat");
    assert_eq!(chat.class, TaskClass::Single);
    assert_eq!(chat.status, AcceptStatus::Accepted);
    assert_eq!((chat.current, chat.target), (3, 9));
    assert_eq!(chat.reward_credit, 250);
    assert_eq!(chat.reward_energy, 7);

    // 缺省字段退回目录值：target / reward / accept_status 都由目录或默认口径补。
    let canvas = &parsed[1];
    let spec = task_catalog::growth_spec("create_canvas").expect("catalog entry");
    assert_eq!(canvas.target, spec.target);
    assert_eq!(canvas.reward_credit, spec.reward);
    assert_eq!(canvas.status, AcceptStatus::NotAccepted);
    assert_eq!(canvas.current, 0);
    assert_eq!(canvas.reward_energy, 0);
}

#[test]
fn parse_list_drops_rows_without_a_task_code() {
    let body = json!({"code": 0, "data": {"tasks": [
        {"title": "no code"},
        {"task_code": "   "},
        {"task_code": "chat_5"},
    ]}});
    let parsed = parse_list(&body);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].code, "chat_5");

    // 没有 data / tasks 时是空清单，不是 panic。
    assert!(parse_list(&json!({"code": 0})).is_empty());
    assert!(parse_list(&json!({"code": 0, "data": {}})).is_empty());
}

// ---------------------------------------------------------------------------
// 顺序与跳过
// ---------------------------------------------------------------------------

#[test]
fn order_puts_auto_class_before_single() {
    let parsed = parse(vec![
        raw("chat_5", "single", "not_accepted", 0),
        raw("first_buddy", "auto", "not_accepted", 0),
        raw("expert_5", "single", "not_accepted", 0),
    ]);
    let ordered: Vec<&str> = order_for_run(&parsed)
        .iter()
        .map(|task| task.code.as_str())
        .collect();
    assert_eq!(ordered, ["first_buddy", "chat_5", "expert_5"]);
}

#[test]
fn first_buddy_prerequisite_holds_single_tasks_back() {
    let parsed = parse(vec![
        raw("chat_5", "single", "not_accepted", 0),
        raw(FIRST_BUDDY, "auto", "not_accepted", 0),
    ]);
    let plan = plan_accept(&parsed, &TaskDayEntry::default(), NOW);
    assert_eq!(plan.blocked_by.as_deref(), Some(FIRST_BUDDY));
    assert_eq!(plan.auto_first, vec![FIRST_BUDDY.to_string()]);
    assert!(plan.batches.is_empty(), "前置未满足时不许接 single");
    assert_eq!(
        reasons(&plan),
        vec![
            (FIRST_BUDDY, SkipReason::AutoNoAccept),
            ("chat_5", SkipReason::PrerequisiteFirstBuddy),
        ]
    );

    // first_buddy 一旦完成（清单里 1/1），single 立刻可以接。
    let done = parse(vec![
        raw("chat_5", "single", "not_accepted", 0),
        raw(FIRST_BUDDY, "auto", "not_accepted", 1),
    ]);
    let plan = plan_accept(&done, &TaskDayEntry::default(), NOW);
    assert_eq!(plan.blocked_by, None);
    assert_eq!(plan.codes(), vec!["chat_5"]);

    // 台账说今天已经领过 first_buddy 时同样放行。
    let mut entry = TaskDayEntry::default();
    entry.claimed.insert(FIRST_BUDDY.into(), ClaimedTask::default());
    let plan = plan_accept(&parsed, &entry, NOW);
    assert_eq!(plan.blocked_by, None);
    assert_eq!(plan.codes(), vec!["chat_5"]);
}

#[test]
fn first_buddy_is_ready_when_the_list_no_longer_offers_it() {
    // 老账号的清单里不会一直带着 first_buddy：把它当「未就绪」会永久卡死所有 single 任务。
    let parsed = parse(vec![raw("chat_5", "single", "not_accepted", 0)]);
    assert!(first_buddy_ready(&parsed, &TaskDayEntry::default()));
}

#[test]
fn night_task_is_skipped_outside_the_window_and_accepted_inside() {
    let parsed = parse(vec![
        raw("black_cat", "single", "not_accepted", 0),
        raw("chat_5", "single", "not_accepted", 0),
    ]);
    let out_of_window = plan_accept(&parsed, &TaskDayEntry::default(), NOW);
    assert_eq!(
        reasons(&out_of_window),
        vec![("black_cat", SkipReason::NightOutsideWindow)]
    );
    assert_eq!(out_of_window.codes(), vec!["chat_5"]);

    let in_window = plan_accept(&parsed, &TaskDayEntry::default(), NIGHT);
    assert_eq!(in_window.codes(), vec!["black_cat", "chat_5"]);
    assert!(in_window.skipped.is_empty());
}

#[test]
fn needs_human_tasks_never_enter_accept_batches() {
    let parsed = parse(vec![
        raw("wb_wechat_oa_subscribe_task", "single", "not_accepted", 0),
        raw("Buddy_App", "single", "not_accepted", 0),
        raw("chat_5", "single", "not_accepted", 0),
    ]);
    let plan = plan_accept(&parsed, &TaskDayEntry::default(), NOW);
    assert_eq!(plan.codes(), vec!["chat_5"]);
    assert_eq!(plan.needs_human.len(), 2);
    assert!(plan
        .needs_human
        .iter()
        .all(|item| item.reason == SkipReason::NeedsHuman && !item.detail.is_empty()));
    assert!(plan
        .needs_human
        .iter()
        .all(|item| !plan.skipped.iter().any(|skip| skip.code == item.code)));
}

#[test]
fn claimed_accepted_and_known_human_tasks_are_skipped() {
    let mut entry = TaskDayEntry::default();
    entry
        .claimed
        .insert("chat_5".into(), ClaimedTask {
            credit: 100,
            at_ms: NOW,
        });
    entry
        .needs_human
        .insert("template_5".into(), "关注公众号满 24 小时".into());
    let parsed = parse(vec![
        raw("chat_5", "single", "not_accepted", 0),
        raw("expert_5", "single", "already_accepted", 0),
        raw("template_5", "single", "not_accepted", 0),
        raw("create_canvas", "single", "not_accepted", 0),
    ]);
    let plan = plan_accept(&parsed, &entry, NOW);
    assert_eq!(plan.codes(), vec!["create_canvas"]);
    assert_eq!(
        reasons(&plan),
        vec![
            ("chat_5", SkipReason::AlreadyClaimed),
            ("expert_5", SkipReason::AlreadyAccepted),
            ("template_5", SkipReason::NeedsHuman),
        ]
    );
    assert!(plan.needs_human.is_empty(), "台账已判真人，不再重复标记");
}

#[test]
fn unknown_codes_are_skipped_instead_of_wasting_an_accept_slot() {
    let parsed = parse(vec![
        raw("brand_new_task_2026", "single", "not_accepted", 0),
        raw("chat_5", "single", "not_accepted", 0),
    ]);
    let plan = plan_accept(&parsed, &TaskDayEntry::default(), NOW);
    assert_eq!(plan.codes(), vec!["chat_5"]);
    assert_eq!(
        reasons(&plan),
        vec![("brand_new_task_2026", SkipReason::UnknownCode)]
    );
}

#[test]
fn attempts_exhausted_stops_every_task_for_the_day() {
    let parsed = parse(vec![
        raw("chat_5", "single", "not_accepted", 0),
        raw("expert_5", "single", "not_accepted", 0),
    ]);
    let mut entry = TaskDayEntry::default();
    entry.attempts = crate::modules::task_ledger::MAX_ATTEMPTS_PER_DAY;
    let plan = plan_accept(&parsed, &entry, NOW);
    assert!(plan.batches.is_empty());
    assert!(plan
        .skipped
        .iter()
        .all(|item| item.reason == SkipReason::AttemptsExhausted));
}

#[test]
fn accept_batches_never_exceed_twenty_codes() {
    let codes: Vec<String> = GROWTH_TASKS
        .iter()
        .filter(|spec| spec.kind.is_some() && !spec.night_only)
        .filter(|spec| !spec.is_unforgeable() && !spec.is_desktop_only())
        // first_buddy 单独留出来：它当前没完成时会把所有 single 前置拦住（那是另一条用例）。
        .filter(|spec| spec.code != FIRST_BUDDY)
        .map(|spec| spec.code.to_string())
        .collect();
    assert!(
        codes.len() > ACCEPT_CHUNK,
        "目录里够 20 条以上的可接任务才有意义：{} 条",
        codes.len()
    );
    let parsed = parse(
        codes
            .iter()
            .map(|code| raw(code, "single", "not_accepted", 0))
            .collect(),
    );
    let plan = plan_accept(&parsed, &TaskDayEntry::default(), NOW);
    assert_eq!(plan.batches.len(), 2);
    assert_eq!(plan.batches[0].len(), ACCEPT_CHUNK);
    assert_eq!(plan.batches[1].len(), codes.len() - ACCEPT_CHUNK);
    assert_eq!(plan.codes(), codes.iter().map(String::as_str).collect::<Vec<_>>());

    assert_eq!(chunk(&[], 0), Vec::<Vec<String>>::new());
    assert_eq!(chunk(&["a".to_string()], 0), vec![vec!["a".to_string()]]);
}

// ---------------------------------------------------------------------------
// 间隔
// ---------------------------------------------------------------------------

#[test]
fn gap_takes_the_stricter_of_engine_and_risk() {
    // 风控关着：引擎的 1s 生效，风控缺省 5s 不能偷偷生效（否则开关等于没关）。
    assert_eq!(step_gap_ms(ENGINE_GAP_MS, false, 5_000, 0), ENGINE_GAP_MS);
    // 风控开着：取更严的那个。
    assert_eq!(step_gap_ms(ENGINE_GAP_MS, true, 5_000, 0), 5_000);
    assert_eq!(step_gap_ms(3_000, true, 1_000, 0), 3_000);
}

#[test]
fn jitter_stays_within_twenty_five_percent_and_never_drops_below_one_second() {
    for pct in -25..=25 {
        let gap = step_gap_ms(5_000, true, 0, pct);
        assert!((3_750..=6_250).contains(&gap), "pct={pct} gap={gap}");
        // 1s 基准：负抖动不许把它压到 1s 以下（上游按行为计时，spec §2.2 #18）。
        assert!(step_gap_ms(ENGINE_GAP_MS, false, 0, pct) >= ENGINE_GAP_MS);
        assert!(step_gap_ms(ENGINE_GAP_MS, false, 0, pct) <= ENGINE_GAP_MS * 5 / 4);
    }
    // 超出 ±25% 的入参被夹住，不会被当成合法的巨大抖动。
    assert_eq!(step_gap_ms(5_000, true, 0, -900), 3_750);
    assert_eq!(jitter_pct(0), -25);
    assert_eq!(jitter_pct(25), 0);
    assert_eq!(jitter_pct(50), 25);
    // 同一种子永远同一结果（确定性抖动，边界才钉得住）。
    assert_eq!(jitter_pct(7), jitter_pct(7));
}

// ---------------------------------------------------------------------------
// 接取响应
// ---------------------------------------------------------------------------

#[test]
fn accept_response_classifies_accepted_already_accepted_and_missing() {
    let requested: Vec<String> = ["chat_5", "expert_5", "create_canvas"]
        .iter()
        .map(|code| code.to_string())
        .collect();
    let body = accept_body(vec![
        accepted("chat_5"),
        json!({"task_code": "expert_5", "status": "already_accepted"}),
        json!({"task_code": "create_canvas", "status": "failed", "message": "nope"}),
    ]);
    let batch = parse_accept(&body, &requested);
    assert_eq!(batch.accepted, vec!["chat_5", "expert_5"]);
    assert_eq!(batch.failed, vec!["create_canvas"]);
    assert!(batch.error.is_none());
    assert!(!batch.ok());

    // 发了但响应里没提 ⇒ 也算失败（参考实现同样把它计入 failed）。
    let partial = parse_accept(
        &accept_body(vec![accepted("chat_5")]),
        &requested,
    );
    assert_eq!(partial.accepted, vec!["chat_5"]);
    assert_eq!(partial.failed, vec!["expert_5", "create_canvas"]);

    // 整批被拒：全部失败 + 一条脱敏原因。
    let rejected = parse_accept(&json!({"code": 400, "msg": "bad request"}), &requested);
    assert_eq!(rejected.failed.len(), 3);
    assert_eq!(rejected.accepted.len(), 0);
    assert!(rejected.error.expect("error").contains("bad request"));
}

#[test]
fn accept_errors_never_leak_credentials() {
    let token = "sk-live-0123456789abcdef";
    let body = json!({
        "code": 401,
        "message": format!("invalid credential Authorization: Bearer {token}"),
    });
    let batch = parse_accept(&body, &["chat_5".to_string()]);
    let text = batch.error.clone().expect("error");
    assert!(!text.contains(token), "错误里漏了凭据：{text}");
    assert!(text.contains("401"));
}

// ---------------------------------------------------------------------------
// 整轮流水线
// ---------------------------------------------------------------------------

#[tokio::test]
async fn round_fetches_accepts_then_refetches_once() {
    let (_dir, ledger) = ledger();
    let io = FakeTaskIo::new();
    io.reply(
        200,
        list_body(vec![
            raw("chat_5", "single", "not_accepted", 0),
            raw("expert_5", "single", "not_accepted", 0),
        ]),
    );
    io.reply(200, accept_body(vec![accepted("chat_5"), accepted("expert_5")]));
    io.reply(
        200,
        list_body(vec![
            raw("chat_5", "single", "accepted", 0),
            raw("expert_5", "single", "accepted", 0),
        ]),
    );

    let round = run_accept_round(&io, &ledger, &account(), Region::Cn, &cfg())
        .await
        .expect("round");

    assert_eq!(round.gate, None);
    assert!(round.ok(), "不该有失败：{:?}", round.failed);
    assert_eq!(round.accepted, vec!["chat_5", "expert_5"]);
    assert!(!round.retried);
    assert_eq!((round.fetched, round.refetched), (2, 1));
    assert_eq!(round.actions, 1);
    assert_eq!(round.batches.len(), 1);
    assert_eq!(round.final_tasks.len(), 2);

    let calls = io.calls();
    assert_eq!(calls.len(), 3, "GET + POST + GET（复读）");
    assert_eq!(calls[0].method, "GET");
    assert_eq!(calls[0].url, format!("{}{TASKS_PATH}", base()));
    assert_eq!(calls[1].method, "POST");
    assert_eq!(calls[1].url, format!("{}{ACCEPT_PATH}", base()));
    assert_eq!(
        calls[1].body,
        Some(json!({"task_codes": ["chat_5", "expert_5"]}))
    );
    assert_eq!(calls[2].method, "GET");

    // 台账：只记了「发了一个写请求」，没有凭空记失败。
    let entry = ledger.entry(&key()).expect("entry");
    assert_eq!(entry.actions, 1);
    assert_eq!(entry.attempts, 0);
    assert!(entry.claimed.is_empty(), "领奖不在本轮");
}

#[tokio::test]
async fn round_retries_exactly_once_when_the_first_accept_fails() {
    let (_dir, ledger) = ledger();
    let io = FakeTaskIo::new();
    io.reply(200, list_body(vec![raw("chat_5", "single", "not_accepted", 0)]));
    // 第一次接取：单条失败。
    io.reply(
        200,
        accept_body(vec![json!({"task_code": "chat_5", "status": "failed", "message": "busy"})]),
    );
    // 复读：还是没接上。
    io.reply(200, list_body(vec![raw("chat_5", "single", "not_accepted", 0)]));
    // 重试一次：成功。
    io.reply(200, accept_body(vec![accepted("chat_5")]));
    io.reply(200, list_body(vec![raw("chat_5", "single", "accepted", 0)]));

    let round = run_accept_round(&io, &ledger, &account(), Region::Cn, &cfg())
        .await
        .expect("round");

    assert!(round.retried, "第一次没接上就该重试一次");
    assert_eq!(round.accepted, vec!["chat_5"]);
    assert_eq!(round.actions, 2);
    assert_eq!(round.fetched, 3);

    let posts = io
        .calls()
        .into_iter()
        .filter(|call| call.method == "POST")
        .count();
    assert_eq!(posts, 2, "总共只许两次接取：首轮 + 重试一次");
}

#[tokio::test]
async fn round_does_not_retry_when_nothing_is_left_to_accept() {
    let (_dir, ledger) = ledger();
    let io = FakeTaskIo::new();
    io.reply(200, list_body(vec![raw("chat_5", "single", "already_accepted", 0)]));
    let round = run_accept_round(&io, &ledger, &account(), Region::Cn, &cfg())
        .await
        .expect("round");
    assert_eq!(io.call_count(), 1, "没有要接的码：连 accept 都不发");
    assert_eq!(round.actions, 0);
    assert!(!round.retried);
    assert_eq!(
        round
            .plan
            .skipped
            .iter()
            .map(|item| item.reason)
            .collect::<Vec<_>>(),
        vec![SkipReason::AlreadyAccepted]
    );
}

#[tokio::test]
async fn round_persists_needs_human_and_holds_single_tasks_behind_first_buddy() {
    let (_dir, ledger) = ledger();
    let io = FakeTaskIo::new();
    io.reply(
        200,
        list_body(vec![
            raw("wb_wechat_oa_subscribe_task", "single", "not_accepted", 0),
            raw(FIRST_BUDDY, "auto", "not_accepted", 0),
            raw("chat_5", "single", "not_accepted", 0),
        ]),
    );

    let round = run_accept_round(&io, &ledger, &account(), Region::Cn, &cfg())
        .await
        .expect("round");

    // 一条写请求都不该发出：需真人 + 前置未满足。
    assert_eq!(io.call_count(), 1);
    assert_eq!(round.actions, 0);
    assert_eq!(round.plan.blocked_by.as_deref(), Some(FIRST_BUDDY));
    assert_eq!(round.plan.auto_first, vec![FIRST_BUDDY.to_string()]);

    let entry = ledger.entry(&key()).expect("entry");
    assert!(
        entry.needs_human.contains_key("wb_wechat_oa_subscribe_task"),
        "需真人要落台账，下一轮才不用重判"
    );
    assert_eq!(entry.actions, 0);
    assert_eq!(entry.attempts, 0);
}

#[tokio::test]
async fn round_records_a_failed_batch_as_one_attempt() {
    let (_dir, ledger) = ledger();
    let io = FakeTaskIo::new();
    io.reply(200, list_body(vec![raw("chat_5", "single", "not_accepted", 0)]));
    io.reply(200, json!({"code": 500, "message": "upstream error"}));
    io.reply(200, list_body(vec![raw("chat_5", "single", "not_accepted", 0)]));
    io.reply(200, json!({"code": 500, "message": "upstream error"}));
    io.reply(200, list_body(vec![raw("chat_5", "single", "not_accepted", 0)]));

    let round = run_accept_round(&io, &ledger, &account(), Region::Cn, &cfg())
        .await
        .expect("round");

    assert!(!round.ok());
    assert_eq!(round.accepted.len(), 0);
    assert!(round.error.expect("error").contains("upstream error"));
    // 首轮 + 重试一次，两批都算失败。
    assert_eq!(round.actions, 2);

    let entry = ledger.entry(&key()).expect("entry");
    assert_eq!(entry.actions, 2);
    assert_eq!(entry.attempts, 2);
    assert!(
        entry.last_error.contains("upstream error"),
        "last_error={}",
        entry.last_error
    );
}

#[tokio::test]
async fn enterprise_and_non_cn_accounts_never_send_a_request() {
    let (_dir, ledger) = ledger();
    for region in [Region::Cn, Region::Global] {
        let mut acc = account();
        if region == Region::Cn {
            acc["enterpriseId"] = json!("ent-42");
        }
        let io = FakeTaskIo::new();
        let round = run_accept_round(&io, &ledger, &acc, region, &cfg())
            .await
            .expect("round");
        assert!(round.gate.is_some(), "闸门要给出跳过原因");
        assert_eq!(io.call_count(), 0);
        assert_eq!(round.actions, 0);
    }
}
