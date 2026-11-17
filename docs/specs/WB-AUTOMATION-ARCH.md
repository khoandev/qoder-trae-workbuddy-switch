> 状态：现行（2026-11-16）

# WorkBuddy CN 自动化：任务完成引擎与反滥用前置

**参考基线**：`wb2api` @ `22b7966`（2026-11-16，Python 实现）。本文的每条对账、每个常量都以该提交为准；
引用格式 `wb_tasks.py:426` 均指该提交的行号。参考仓库不在本仓内，读完本文即可，不需要它的 checkout。

**相关文档**：[`DECISIONS.md`](DECISIONS.md) D58–D60 · [`VOCABULARY.md`](VOCABULARY.md) §3（能力 `tasks`）·
[`RISK-CONTROLS-DESIGN.md`](RISK-CONTROLS-DESIGN.md) · [`RISK-ACCOUNT-ISOLATION.md`](RISK-ACCOUNT-ISOLATION.md) ·
[`ACCOUNT-FAILOVER.md`](ACCOUNT-FAILOVER.md)（网关侧换号，与本文的账号侧自动化是两条链路）

## 1. 一句话与边界

把「账号自己会做的成长任务」搬到本应用的调度器里：**读任务 → 批量接取 → 按任务类型上报事件 →
复读进度 → 领奖 → 落台账**，并让设备身份 / 账号出口 / 地区面板 / TLS 指纹这四类**反滥用前置**
作为**按账号可选**的开关存在。

三条边界，先写在最前面（UI 文案与后端注释都受它约束）：

1. **不承诺规避封禁**。本节所有开关只改变「本软件发出的请求长什么样、从哪出去」，
   **不保证**账号不被限制、不被封、不被判定关联；封禁判定由平台做出，本应用不推断原因。
2. **不伪造必须真人完成的任务**。桌面专属、需真实捐款、需关注公众号满 24 小时这类任务
   **一律标 `needs_human` 并给出深链**，不上报伪造事件（见 §6）。
3. **默认全关**。任务自动完成、设备身份派生、账号出口、地区面板这四类开关
   **默认都是关的**，开启粒度是**单个账号**，并且必须能**立即关掉**（下一轮不再排程，见 §5）；
   TLS 指纹本轮**不实现**，差额已实测并记入 §7.5（选择 narrowing，理由见该节）。

## 2. 与 `wb2api@22b7966` 的对账

「本仓现状」列是 2026-11-16 的实测状态（`rg` 全仓确认，不是记忆）。

### 2.1 任务完成

| # | 能力 | wb2api 参考 | 本仓现状 | 本期落点 |
|:--|:--|:--|:--|:--|
| 1 | 任务清单 | `GET /v2/activity/growth/tasks`（桌面头）；`TASK_SPECS` 27 条（`wb_tasks.py:137`；本仓 2026-11-16 实测该 dict 有 27 个键，早前文档写的 24 是错的）+ 清单带 `ex_*` 时动态解析专家（`wb_tasks.py:481`） | `cat.rs:29` 只取同一端点筛 `black_cat` | 通用引擎（§3.6），`feature=growth_tasks`；元数据在 `task_catalog.rs`（§3.4） |
| 2 | 批量接取 | `POST /v2/activity/growth/tasks/accept`，体 `{"task_codes":[…]}`，每批 20；逐批记录 `accepted` / `already_accepted` / `failed`，失败**不吞**（`wb_tasks.py:359`） | 只接 `black_cat` | `task_engine`（§3.6）：分批 + 复读 + 只重试一次，失败逐条可见 |
| 3 | 事件上报 | `POST /v2/report`；上游按 `(eventCode, id)` 去重 ⇒ 专家/团队 id 必须互异（`wb_tasks.py:1210`），`EXPERT_ID_POOL` 18 条（`:234`）、`TEAM_ID_POOL` 8 条 | `activity.rs` 的 `/v2/report` 构造器只有对话/心跳类事件 | 收口到 `task_event`（§3.5）：按 `kind` 生成，`(eventCode, id)` 每次尝试互异 |
| 4 | 复读进度再领奖 | 上报后最多 6 次复读（1.5s / 涨了再 2.5s），未达 `target` 则**顺延到下一轮**（`wb_tasks.py:1241`–`1252`） | `cat.rs` 有同类轮询 | 固化为 verify-before-claim（§4） |
| 5 | 领奖 | `POST /activity/growth/tasks/{code}/claim`，体 `{}`；`400 task not completed` 不当致命错（`wb_tasks.py:426`） | `black_cat` 有 | 通用 |
| 6 | 小程序族 | `MP_TASK_SPECS`（`Sequential_Tasks_1..6` + `school_season`，`wb_tasks.py:52`）；头 `X-Client-Product: workbuddy-mp`、`X-Client-Version: 2.4.0`、`X-Client-Platform: mp-weixin`、`X-Platform: wechatmp` + 微信小程序 UA（`:38`–`:51`） | 无（`school.rs` 只做校园日） | 独立 `feature=mp_tasks` |
| 7 | 运营活动族 | 独立契约 `/portal/activity/*`：`campaign_starbucks_2026`（对话 3 次 / 召唤专家 / 解锁 App）、`campaign_reporter_2026`（专家 / 技能 / 桌面灵感）；节流 3s、轮询 6s、慢档 10s、领奖 4s/5s、桌面等待 60s（`wb_tasks.py:66`–`74`） | 无（`qoder/campaign.rs` 属 Qoder） | 独立 `feature=campaign`，常量照搬 |
| 8 | 真实动作类 | `skill_share` 先建 c2c 分享渠道再上报渠道动作（`wb_tasks.py:190`）；`template_5` 用服务端场景 id + 四事件组（`:198`） | 无 | `growth_tasks` 的子流程 |
| 9 | 猫猫旅行 | `GET /activity/growth/buddy/travel/status` + `POST …/travel/claim`；`first_buddy` 是所有 single 任务与旅行的前置（`wb_tasks.py:795`） | `travel.rs` 已有派出/领奖循环 | 复用，补 `first_buddy` 前置判定 |
| 10 | Buddy 盲盒 | `GET /v2/activity/growth/buddy/quota` + `POST /v2/activity/growth/buddy/open`（`wb_tasks.py:1276`） | 无 | `feature=buddy_box` |
| 11 | 连登 / 补签 / 抽奖 / 热力 | `streak` / `makeup-cards/use` / `redeem` / `lottery/*` / `heatmap` | `activity.rs` 全部已有 | **不动** |
| 12 | 夜间任务 | `black_cat` 只在 23:00–08:00 计数，每天 1 次、累计 3 天（`wb_tasks.py:219`、`:225`） | `cat.rs` 已有窗口判定 | **不动** |

### 2.2 反滥用前置

| # | 机制 | wb2api 参考 | 本仓现状 | 本期落点 |
|:--|:--|:--|:--|:--|
| 13 | 设备身份派生 | `wb_fingerprint.derive_id`：账号 UID + 本地盐 → 稳定 `machineId` / `sessionId` / `requestId`，多账号天然隔离（`wb_fingerprint.py:1`） | 一律用官方客户端原值 | opt-in `device_identity`（§7.1） |
| 14 | 账号出口（代理槽） | `proxy_slots`：`slot-N` + `url` / `enabled` / `ip` / `country` / `asn` / `probed_at`，账号侧持久绑定 `proxySlot`，槽 id **单调分配、不复用**（`wb_settings.py:3440`） | 三平台共享出站客户端 | opt-in `egress_slot`（§7.2） |
| 15 | 出口退避 | `wb_egress` + `egress_key`（scheme/host/port/proxy 折叠成一把 key），失败计数**按出口**共享而不是按账号（`wb_upstream_pool.py:1720`） | 只有 `risk_control` 的账号级冷却 | opt-in `egress_backoff`（§7.2） |
| 16 | 地区面板 | `wb_client_geo` + `wb_ipintel`：按出口 IP 生成地区/时区/语言头 | 无 | opt-in `client_geo`（§7.3） |
| 17 | TLS 指纹 | `WB_TLS_FINGERPRINT=auto/node/python`（默认 auto）+ 独立 Node 桥进程；`python` 表示不起桥（`wb_upstream_pool.py:133`） | rustls 默认栈 | 差额已实测并记入 §7.5；本轮**不挂桥**（narrowing） |
| 18 | 请求间隔 | 任务链 ≥ 1.0s；活动族按批 3s/4s/10s | `risk_control`（默认关：30 rpm、账号间隔 5s） | 每条 feature 自带间隔，统一走 `admit_platform_request`（§3.2） |

**对账结论**：缺口集中在 §2.1 的 1–8、10 与 §2.2 的 13–16。`activity.rs` / `travel.rs` / `cat.rs`
已有的口径（连登链只服务 CN、`/v2/report` 的 GET/POST 分野、夜间窗口）**不在本期重写**。

## 3. 架构

```
scheduler（每日窗口 + 每账号开关）
        │
        ▼
automation::run_account(account)        ← 一轮一个账号，串行
        │  feature = growth_tasks | mp_tasks | campaign | buddy_box
        ▼
TaskIo（trait）  ── 唯一出网点 ──►  risk_control::admit_platform_request
   │   get_json / post_json              ↓ 成功后 record_platform_response
   │   headers(feature, region)       ledger（§4）── 决定今天还做不做
   ▼
task kernel（纯函数）
   plan(清单, 台账) → Vec<Action>        Accept / Report / Claim / NeedsHuman / Skip
   verify(再次清单, 预期) → bool
```

### 3.1 三层职责

| 层 | 位置（本期计划） | 只做这些 |
|:--|:--|:--|
| 内核 | `crates/buddy-switch-core/src/modules/wb_tasks/`（`plan.rs` / `event.rs`）+ `task_catalog.rs`（任务元数据，§3.4） | **纯函数**：从清单 + 台账算出 `Vec<Action>`，不碰网络、不看时钟（`now` 由调用方注入） |
| 写出 | `crates/buddy-switch-core/src/modules/task_io.rs`（`TaskIo` / `RealTaskIo` / `FakeTaskIo`） | 每个请求都先 `admit_platform_request(product, region, uid)`，被拦返回 `status=0, code=-3`；响应回写 `record_platform_response`；`RiskPermit` 活到请求结束 |
| 台账 | `crates/buddy-switch-core/src/modules/task_ledger.rs`（`TaskLedger` / `TaskDayKey` / `TaskGuard`） | 当日幂等、失败预算 3 次、`needs_human` 中性状态、30 天裁剪；文件损坏即 fail-closed |
| 调度 | `scheduler.rs` + `schedule.rs` | 何时跑、跑哪个账号、失败后退到哪天；**不判断任务语义** |

**为什么内核必须是纯函数**：任务链的判据全在「清单长什么样 + 台账里今天做过什么」，
把它做成纯函数才能在**离线**测试里覆盖 24+7 条 `TASK_SPECS` 与两张活动表，而不需要任何账号。

### 3.2 一律走风控准入

- 所有写请求（accept / report / claim / buddy open / campaign 动作）都经
  `risk_control::admit_platform_request("workbuddy", "cn", account_id)`；被拒（暂停 / 冷却 /
  限速）即**当轮停止该账号**，不换号接力（与 `RISK-CONTROLS-DESIGN.md` §故障处置一致）。
- 响应回写 `risk_control::record_platform_response`：`401/403` → 账号暂停，`429` → 冷却。
- 任务链自带的最小间隔（≥ 1.0s）是**第二道**闸：风控关掉时它仍然生效，因为上游按行为计时，
  与本软件的设置无关。
- **已收口**：`activity.rs` / `cat.rs` / `school.rs` 的真实实现只经 `task_io::send`
  （准入 → 请求 → 结果回写），文件里不再出现 `authed_json_request_for` / `reqwest::`；
  回归由源码级单测 `task_io::tests::task_modules_never_call_http_directly` 拦 ——
  它用 `include_str!` 把这三个文件读进编译产物，谁再写回直连就红。
- **尚未并入 seam**：`travel.rs` 仍自己调 `risk_control`（准入与回写都在，但没走 `TaskIo`），
  在 travel 迁移步里一并收口；迁移期 `TaskIo` 与既有 `CatIo` / `SchoolIo` 两套缝并存。

### 3.3 每账号开关（`AccountsPage`）

数据放在账号记录的 `automation` 字段（缺省即全关）：

```jsonc
"automation": {
  "tasks": false,            // §2.1 的 1–8、10 总闸（能力 tasks）
  "features": { "growth_tasks": false, "mp_tasks": false, "campaign": false, "buddy_box": false },
  "device_identity": false,  // §7.1
  "egress_slot": null,       // §7.2，null = 直连/系统代理
  "egress_backoff": false,   // §7.2
  "client_geo": false        // §7.3
}
```

- 总闸与分项**都与**关系：`tasks=false` 时任何 feature 都不跑，但分项记忆保留（再打开时不必重设）。
- 关闭**立即生效**：调度器每轮开头重读账号记录，不缓存开关（`app_prefs` 同样的纪律）。
- UI 文案只能写「减少过密请求 / 固定出口 / 地区一致性」这类**可验证**的话；
  **不许**出现「防风控」「避免封号」「规避关联」等承诺（§1 第 1 条）。

### 3.4 任务目录（`task_catalog`，唯一一份元数据）

`crates/buddy-switch-core/src/modules/task_catalog.rs` 是任务元数据的**唯一**落点：
runner 里不许再出现任务码字面量、`target` 数字、专家 id。内容全部转自
`wb_tasks.py`（`MP_TASK_SPECS:52` / `CAMPAIGN_SPECS:74` / `TASK_SPECS:137` /
`DESKTOP_ONLY_TASKS:219` / `NIGHT_TASK_CODES:225` / `EXPERT_ID_POOL:234` / `TEAM_ID_POOL:246`）。

| 表 | 条数 | 说明 |
|:--|:--|:--|
| `GROWTH_TASKS` | **27** | 每条带 `kind`（16 种判定器）、`target` / `reward`、可选 `expert`、三个旗标 |
| `MP_TASKS` | **7** | `Sequential_Tasks_1..6` + `school_season`；`kind` 决定小程序事件构造器 |
| `CAMPAIGNS` | **2** | 星巴克协作、记者节；内含子任务与 `expert` / `skill` / `case_id` 载荷 |
| `EXPERT_POOL` / `TEAM_POOL` | **18 / 8** | 事件 id 池：上游按 `(eventCode, id)` 去重，重复 id 会让进度永远不动 |

五条判定纪律（都有单测钉住）：

1. **服务端优先**：名称 / `target` / `reward` 的服务端值覆盖目录值（`title or spec.name` 同序），
   目录只给缺省与语义旗标。
2. **未知任务码 = 跳过**：目录没有、也推不出专家 id ⇒ `SpecSource::Unknown`，
   `kind` 为 `None`、`is_actionable()==false`。**不猜 kind**：猜错等于给新任务码上报无效事件。
3. **旗标带理由**：`desktop_only` / `unforgeable` 是 `Option<&'static str>`（提示文案），
   不是裸 `bool`——「是不是」与「为什么」不可能各说各话；`night_only` 只有 `black_cat`。
4. **动态专家优先合法 id**：`jump_url` 的 `deeplink` 内层 `expertId` → 外层 `expertId` →
   `expert_use_<slug>` 前缀；id 必须匹配 `^ex_[A-Za-z0-9]{6,64}$`，脏 id 不许透传。
   静态目录先命中，所以 `expert_use_maidehao` 用的仍是 `ex_GDl31WahyIH0`。
5. **企业号 / 海外区一律跳过**：`gate_for(region, account)` 先看 `enterpriseId`
   （判据同 `is_enterprise`；JWT 旁路不做），再看 `region != Cn`，两者都返回跳过原因字符串。

验证：`cargo test -p buddy-switch-core --lib modules::task_catalog`（16 条，含目录规模、
顺序钉死、逐条点检表、未知码、动态专家、脏 id、服务端覆盖、闸门、活动节流、id 池回绕、
以及「目录里没有凭据」的 include_str! 守卫）。

### 3.5 任务事件（`task_event`，唯一一份事件形状）

`crates/buddy-switch-core/src/modules/task_event.rs` 是事件体的**唯一**落点：runner 只决定
「发哪个 kind、第几次尝试」，形状与字段名全部从这里出。转自 `wb_tasks.py` 的
`build_event:640` / `build_desktop_chat_events:508` / `_template_events:1078` /
`_skill_share_channel_events:976` / `mp_base:549` / `_campaign_chat_event:1566` /
`_campaign_expert_events:1584` / `_campaign_skill_events:1611` / `_campaign_desktop_events:1637` /
`CAMPAIGN_CASES:111`。

| 入口 | 覆盖 |
|:--|:--|
| `desktop(kind, user, ids)` | canvas / template / expert / team / lighthouse / skill / automation / playbook / skin / chat / glmchat / cat / library，以及 `RichMeow` 的 6 连对话 |
| `skill_share_chain(ids, share_code)` | 两条 `skill_share_channel_action`；`share_code` 必须来自真实建渠道响应 |
| `chat_chain(user, ids, model)` | 桌面 6 连成功对话（`agent_task_created` → … → `agent_task_completed`） |
| `mp(code, ids, expert)` | 小程序单条事件，`kind` 从 `task_catalog::mp_spec` 取；未知码返回 `None` |
| `campaign_chat` / `campaign_expert` / `campaign_skill` / `campaign_desktop` | 活动四类判据 |
| `desktop_envelope` / `mp_envelope` / `with_envelope` | 两份信封；事件字段优先于信封字段 |
| `idempotency_key(event)` | 上游去重键 `(eventCode, id)` |

五条纪律（都有单测钉住）：

1. **纯函数**：不读时钟、不发请求。`now_ms` 与 `attempt` 由调用方给（`EventIds`），
   事件体因此可在测试里逐字断言。
2. **id 每次尝试都不同**：`conversationId` / `requestId` / `messageId` 掺 `attempt`；
   `wbx-canvas-*` 与 `template` 的 `id` 同样掺（参考实现只带毫秒，同毫秒两轮会撞键）。
3. **池内 id 不越界**：专家 / 团队 id 只能来自 `EXPERT_POOL` / `TEAM_POOL`，按 attempt 轮转；
   超出池子后重复是上游契约，靠台账「点亮即停」而不是造一个假专家 id。
4. **信封只有两份**：桌面（`machineId` / `sessionId` / `ideVersion` 由 `device_identity` 与
   `identity` 传入）与小程序（`mp-<uid 前 16 位>`）。本模块不持有安装盐。
5. **不装凭证**：事件体与信封里没有 token / cookie / `Authorization` / 密钥字段，
   `events_never_carry_credential_fields` 逐字段扫描钉住。

验证：`cargo test -p buddy-switch-core --lib modules::task_event`（15 条，含逐 kind 必填字段、
跨轮 id 唯一、id 池轮转、6 连对话、小程序 kind 映射与未知码、两份信封、活动四类判据、
活动页面上下文、去重键、凭证扫描）。

### 3.6 成长任务引擎（`task_engine`：读 → 排序 → 接取）

`crates/buddy-switch-core/src/modules/task_engine.rs` 负责流水线的**前半段**：
读清单（`fetch_growth_tasks:255`）→ 排序 → 每 20 条一批接取（`accept_tasks:359`）→
复读 → **只重试一次**（`run_growth_tasks:1821`）。上报事件与领奖**不在**这里。

```
GET  {chat_base}/v2/activity/growth/tasks
  → plan_accept(清单, 当日台账, now)   ← 纯函数：接什么 / 不接什么
  → POST {chat_base}/v2/activity/growth/tasks/accept  {"task_codes":[…≤20]}
  → GET  …/tasks（复读）→ 仍未接上的码 → 再 POST 一次（到此为止）
```

六条纪律（都有单测钉住）：

1. **auto 类不接取**：上游对 `task_type == "auto"`（`first_buddy` 等）明确
   「task does not require acceptance」。它们只进 `AcceptPlan::auto_first`（事件链先跑），
   一批都不占 —— 把 auto 塞进 accept 只会换来一条失败结果。
2. **`first_buddy` 是 single 与猫猫旅行的前置**：清单里有它而没完成（`current < target` 且
   `accept_status == not_accepted`）时，single 一律不接（`prerequisite_first_buddy`），
   `AcceptPlan::blocked_by` 置 `Some("first_buddy")`。**清单里没有它 = 视为就绪**：
   老账号的清单不会一直带着它，把「不在清单」当未就绪会永久卡死所有 single 任务。
3. **不猜、不白占**：目录不认识的码（`unknown_code`）、今天已领（`already_claimed`）、
   今天已判真人（`needs_human`）、当日失败到顶（`attempts_exhausted`）、已接取
   （`already_accepted`）—— 都**不进批次**。夜间任务（`black_cat`）只在 CST 23:00–08:00
   的窗口内才接，窗口外跳过。
4. **间隔取更严的那个**：`max(引擎 1s, 风控 min_account_gap_ms) + ±25% 抖动`，负抖动
   **不低于 1s**。风控关着时不采信它的 `min_account_gap_ms`（5s 是风控自己的缺省，
   开关关掉还照样 5s 等于开关没生效）。抖动是确定性的（同一种子同一结果），
   边界因此在测试里可以逐值断言。
5. **台账先写后做**：每个 accept 批次发出**之前**先 `record_action`（进程崩了也留「花掉一个
   名额」），整批失败再 `record_failure`（脱敏 + 当日 3 次上限）；`needs_human` 在本轮开头就
   落台账，下一轮连重判都省了。**单条被拒不推进 `attempts`**，只有整批失败才算一次。
6. **失败逐条可见**：`data.results` 里不是 `accepted` / `already_accepted` 的码记失败，
   **发了但响应里没提的码也算失败**（参考实现当年正是吞掉这些失败，才出现「接了一堆却
   一个都没点亮」）。错误文案过 `redact`，不把凭据写进台账。

与参考实现的两处**有意差异**，理由都在上面：夜间任务「窗口内可接」（参考实现整体排除，
交给 `run_night_growth`）、已完成的 auto 任务不再排进 `auto_first`（参考实现会照跑一遍事件）。

验证：`cargo test -p buddy-switch-core --lib modules::task_engine_accept`（21 条，含清单字段
优先级、无码行丢弃、auto 先序、`first_buddy` 前置、夜间窗口内外、需真人/已领/已接/未知码/
失败到顶的跳过、20 条分批、间隔与抖动边界、接取结果三分类、错误脱敏、整轮流水线的
「GET→POST→GET」「只重试一次」「不该发就不发」以及台账 `actions`/`attempts` 计数）。

## 4. 台账与 verify-before-claim

**台账路径**：`~/.buddy-switch/wb_automation_ledger.json`（与 `risk_control.json` 同目录，
`atomic_write` 落盘）。键是 `{product}/{region}/{account_id}/{feature}/{YYYY-MM-DD}`：

```jsonc
{
  "workbuddy/cn/{account_id}/growth_tasks/2026-11-16": {
    "attempts": 2,
    "actions": 11,              // accept/report/claim 的实际请求数
    "claimed": ["chat_5", "expert_5"],
    "needs_human": ["wb_wechat_oa_subscribe_task"],
    "last_error": "claim chat_5: task not completed",
    "updated_at": 1763270400000
  }
}
```

**三条硬规则**：

1. **当日幂等**：同一 `(账号, feature, 任务码)` 当日已 `claimed` 的不再上报、不再领奖；
   已 `needs_human` 的当日不再重试（避免每小时空转一次真动作判定）。
2. **verify-before-claim**：`Claim` 只在**复读清单**显示 `current >= target` 或
   `status ∈ {completed, claimed}` 时发出；复读轮询沿用参考实现（最多 6 次，1.5s / 涨了再 2.5s），
   未达成 → 记 `last_error` 并**顺延到下一轮**，不硬打领奖接口。
3. **当日失败不无限重试**：`attempts` 达到上限（默认 3）后当日停手，`last_error` 落台账
   供账号页展示。`400 task not completed` 属「还没到」而非故障，不推进 `attempts`。

## 5. 默认值

| 开关 | 默认 | 粒度 | 关掉会发生什么 |
|:--|:--|:--|:--|
| `tasks` | **关** | 账号 | 不写任何任务接口（账号页仍可只读查看清单） |
| `device_identity` | **关** | 账号 | 用官方客户端原值，与今日行为一致 |
| `egress_slot` | **关**（`null`） | 账号 | 走共享出站客户端（`GATEWAY-UPSTREAM-NETWORK.md` 的直连/系统代理规则） |
| `egress_backoff` | **关** | 账号 | 只保留 `risk_control` 的账号级冷却 |
| `client_geo` | **关** | 账号 | 不发地区/时区/语言附加头 |
| TLS 指纹 | **narrowing**（不挂 uTLS 桥） | — | 与今日一致：rustls 默认栈，差额见 §7.5 |
| 任务链最小间隔 | **1000ms** | 常量 | 不可关：上游按行为计时 |
| 活动族间隔 | **3000ms**（慢档 10000ms） | 常量 | 同上 |
| 台账 `attempts` 上限 | **3**/日 | 常量 | 当日停手，次日重来 |

**不把任何默认值说成「平台的安全阈值」**：它们只是「不太密」的经验值，与平台实际阈值无关。

## 6. needs-human 策略

「需真人」不是失败，是**明确不做**。判定来源只有三处，且都写进清单：

| 来源 | 例子 | 处置 |
|:--|:--|:--|
| `unforgeable` 标记 | `wb_wechat_oa_subscribe_task`（关注公众号满 24h）、`Expert_Philanthropy`（真实捐款） | 标 `needs_human` + 原因；**不上报**、不领奖、不重试 |
| 桌面专属动作 | `Buddy_App`、`Buddy_App_QQ`（`workbuddy://` 深链，事件会被忽略或落到 heartbeat） | 同上，并给出桌面深链文案 |
| 上游明说需要新条件 | `first_buddy` 未领到（`prerequisite not met`） | 降级为「先做 `first_buddy`」，不当故障 |

- `needs_human` 在账号页显示为**中性徽章**（不是红色错误），点击跳到官方页面；
- 一轮里出现 `needs_human` **不阻止**其它任务继续；
- **禁止**为绕过上面三类去伪造 `jump_url` 之外的信号（例如伪造 `/v2/report` 桌面事件）；
  这条与 §1 第 2 条同义，但值得单列：参考实现 2026-09 已实测，伪造只会让进度永远 0/1。

## 7. 反滥用前置（逐项）

### 7.1 设备身份（`device_identity`）

- 每账号派生的 `machineId` / `sessionId` / `requestId` 由 **账号 UID + 本地盐**经 HKDF-SHA256
  （`sha2` 手写 HMAC，不引入 `hkdf` / `hmac` crate）派生，稳定不变；`info` 里带
  `workbuddy/{realm}/{label}/{nonce}`，因此**同 uid 在两区是两个设备**、每个请求的 id 不重复。
- **实现**：`crates/buddy-switch-core/src/modules/device_identity.rs`。盐落
  `~/.buddy-switch/device_identity_salt.json`（`atomic_write` ⇒ 0600），**不进备份包、不进日志、
  不进导出、不进诊断包**（备份是表驱动分区，这里刻意不加分区）；文件损坏时**报错而不换盐**
  （静默换盐 = 把该账号变成新设备，且不可回滚），派生侧于是 fail-closed 到官方原值。
- **唯一挂载点**：`crates/buddy-switch-core/src/modules/task_io.rs` 的 `outbound_headers`
  （`send` 的唯一头部入口）调 `device_identity::apply`；feature 模块不许自己拼 `x-client-*`，
  否则用户关了开关也仍有路在发旧身份。小程序信封另走 `apply_mp`（`device_id` 按 uid、
  `session_id` 按 uid + 昵称摘要，改昵称换会话不换设备）。
- 只影响本软件自己发出的请求头；**不写官方客户端的身份文件**（与 `RISK-ACCOUNT-ISOLATION.md` 的
  单账号快照纪律一致）。
- 关掉后立刻回到官方原值（`enabled_for` 每轮重读账号 JSON，不缓存开关）；派生值仍在本地保存
  （再打开时是同一个设备，而不是新设备）。
- ⚠️ **头名待定稿**：`device_identity.rs` 的 `HEADER_*` 五个常量（`x-client-machine-id` /
  `x-client-session-id` / `x-request-id` / `x-client-mp-device-id` / `x-client-mp-session-id`）
  是临时口径，等 §9 的在线 spike（两枚 CN 账号）与参考实现逐字对齐后改常量即可（调用方不动）。
  开关默认关，所以今日没有任何请求真的带这些头。
- **测试**：`cargo test -p buddy-switch-core --lib modules::device_identity`。含 RFC 5869
  Test Case 1（证明是真 HKDF）、稳定性 / 跨账号 / 跨区 / 跨标签隔离、UUID v4 形状、默认关的
  空操作、0600 权限、盐损坏 fail-closed，以及 `Debug` 与错误串的脱敏断言。

### 7.2 账号出口（`egress_slot`）与出口退避（`egress_backoff`）

- 槽表是**用户自己的命名代理列表**（`label` / `scheme` + `host` + `port` / 加密凭据 /
  `region` 提示 / `enabled`；落盘时的真实字段集见 §7.2.1），
  账号绑定的是**槽 id**，不是 URL 字符串 —— 改 URL 不改变绑定，删槽必须显式解绑。
- 槽 id **单调分配、不复用**：否则删掉 `slot-2` 再新建会把所有绑 `slot-2` 的账号**静默改道**。
- 出口退避按**出口**而不是按账号计数：同一出口上的失败属于同一条链路；
  与 `risk_control` 的账号级冷却**两层并存**，先取更严的那个。
- 退避与熔断的**具体阶梯**（`crates/buddy-switch-core/src/modules/backoff.rs`，纯内存、无 IO）：

  | 项 | 值 | 说明 |
  |:--|:--|:--|
  | 失败分类 | `transient` / `rate_limit` / `hard` | `5xx`、超时、连接重置、`status=0`（没发出去）= transient；`429` = rate_limit；其余 `4xx` = hard |
  | 指数退避 | `1s → 2s → 4s → 8s → 15s`（上限） | 按**同一任务**的重试次数，与其他任务无关 |
  | 抖动 | `0.75–1.25 ×` 确定值 | 抖动源是 `uuid` 的随机位，不引入 `rand`；有 `Retry-After` 时以服务端为准 |
  | `Retry-After` | 夹在 `[1s, 5min]` | 既不忙等，也不让一个 `86400` 把工作线程钉死一天 |
  | 熔断触发 | **连续 3 次 hard** | transient / rate_limit **不计数**，一次抖动不会熔断 |
  | 熔断窗口 | `30min → 1h → 2h → 4h → 6h`（封顶） | 半开试探失败才翻倍；窗口内的迟到失败只记账不翻倍 |
  | 半开 | 只放**一个**试探请求 | 结果回来前其余请求 `Waiting`；成功则失败史与阶梯一起清零 |
  | 半开超时 | 试探 60s 未回报视作丢失 | 允许再派一个试探，否则一次 panic / 被丢弃的 future 会让该出口永久卡住 |

- **业务态不许进熔断**：`growth` 域的 `403`（天数不足）、`400 task not completed` 属任务结果，
  调用方先收敛再决定要不要算出口失败 —— 否则三次「天数不足」会把整条出口停掉。
- 熔断键是出口，**不是账号**：一条槽熔断不影响同账号在别的槽/直连上的请求，反之亦然。
  这是与 `risk_control` 账号冷却分开的理由：出口故障不该记到账号头上。
- 关掉 `egress_slot` 立即回落共享出站客户端；**保留**槽定义（用户数据不静默删除）。

### 7.2.1 落地状态

- 退避与熔断：`crates/buddy-switch-core/src/modules/backoff.rs`（纯内存、无 IO，键为
  `slot-N` / `direct`）。
- 设备身份：`crates/buddy-switch-core/src/modules/device_identity.rs`（§7.1），挂载点
  `task_io::outbound_headers`。
- **代理凭据（用户名/密码）只存本地文件、只进 reqwest 的 `socks`/`http` 配置**；
  日志与错误串一律打码成 `socks5://***@host:port`（见 §8）。
- 出口槽：`crates/buddy-switch-core/src/modules/egress.rs` —— 槽模型
  （`id` / `label` / `scheme` = `http|https|socks5|socks5h` / `host` / `port` /
  `credential`（at-rest 信封）/ `region` / `enabled`）、`EgressTable`（`slot-N` **单调发号**，
  `next_index` 手改小也不复用）、按槽缓存的 `reqwest::Client`（缓存键 = 槽 id + 代理地址摘要，
  改 URL 自动换客户端）。
- 落盘：槽表 `~/.buddy-switch/egress_slots.json`；凭据密钥
  `~/.buddy-switch/egress_key.json`（0600，随机 32B）。凭证用 `at_rest::seal_field`
  封成**与客户端同格式**的字段信封（AES-256-GCM），明文只在建连那一刻存在于内存。
  两者**都不进备份 / 导出 / 诊断包**（备份是表驱动分区，这里刻意不加分区）；
  密钥文件损坏时**不重新生成**（否则既有密文永久解不开），只报错。
- 与设备身份**分成两把钥匙**：清理设备身份（换指纹）不该让代理凭据变成解不开的密文，反之亦然。
- 账号绑定：`automation.egress_slot`（只存槽 id）。解析规则：未绑定 / 槽被删 /
  槽 `enabled=false` / 槽 id 非法 ⇒ `Resolution::direct()`（**与改造前逐字相同**）；
  绑定存在但解不开（槽表损坏、密钥换了）⇒ **报错，不回落直连** —— 否则同一账号的流量
  会从两个公网出口出现，正好破坏隔离。
- 挂载点（内核侧三条，别再开第四条）：

  | 路径 | 函数 |
  |:--|:--|
  | 任务链写请求（成长 / 小程序 / 活动 / 盲盒） | `task_io::send` → `config::authed_json_request_for` → `config::http_request_raw_for_account` |
  | Token 刷新 | `refresh::refresh_account_token_for` |
  | 签到（状态查询 / 执行） | `checkin::checkin_request_for` |
  | 网关中继 | `upstream::UpstreamClient::client_for_account` |

- **`upstream_via_proxy` 的新地位**：保留为「网关共享兜底出口」（未绑定槽的账号用它，
  行为与改造前一致），不再是唯一出口；账号一旦绑定槽就以槽为准。
- **测试**：`cargo test -p buddy-switch-core --lib modules::egress`。含 scheme / 默认端口、
  槽校验（host 里混凭据、坏端口、只填半个用户名）、`slot-N` 单调且不复用、凭据封存后
  **盘上无明文**且能解回、未绑定默认直连 + 解析失败 fail-closed、绑定一个账号不影响其他账号、
  按槽缓存且改 URL 换客户端、凭据打码（IPv6 也带方括号、URL 里不带凭据）。
- **登录（扫码 / OAuth）不走槽**：那一刻还没有账号记录，也就没有 `automation.egress_slot`
  可读；要覆盖登录流量必须先选槽、后登录，属 UI 侧顺序问题，本期不做。
- **尚未落地**：AccountsPage 的槽管理与绑定 UI、§7.3 的出口 IP 探测（`region` 字段目前
  只用于展示，不参与路由判定）。

### 7.3 地区面板（`client_geo`）

- 由**实际出口 IP** 探测得到地区码 / 时区 / 语言，附加到任务与活动请求头；
  无出口（直连）时用本机时区，不猜测。
- 只在 `client_geo=true` 时附加；与网关侧的 `X-Client-Platform` 等**固定头**互不覆盖。
- 探测失败 ⇒ **不附加**（fail-closed 到「没有地区头」），不用默认值冒充。

### 7.4 出站头部（`outbound_headers`）

- **五种形态各一张表**（`crates/buddy-switch-core/src/modules/outbound_headers.rs`）：
  `desktop`（浏览器形态 JSON）、`cli`（上游刷新 / 目录）、`mp`（微信小程序）、
  `campaign_portal`（活动 web 门户）、`chat`（对话 SSE）。表是**有序**的
  `Vec<(name, value)>`，模块外只能拿 `to_map()`，不许再各自拼字面量。
- **UA / 版本只有一处**：桌面形态用 `config::DEFAULT_HTTP_USER_AGENT`（官网套餐页捕获的
  那条 Chrome UA），CLI 形态用 `outbound_headers::CLI_USER_AGENT`，chat 形态由
  `identity::chat_user_agent(region, version, cli)` 构造。唯一允许写死的是
  `MP_USER_AGENT`——官方小程序 UA 里没有版本 token，也没有可解析来源。
  测试用 `include_str!` 扫 `school.rs` / `activity.rs` / `cat.rs` / `upstream.rs`，
  出现 UA 字面量即红。
- **顺序是构造契约，不是线上字节序证据**：`reqwest` / `http::HeaderMap` 内部是哈希表，
  实际发出的字节顺序不受本表控制。快照（`HeaderSet::wire_snapshot`，`name: value` 每行一条）
  防的是**头名与取值的漂移**；要做到逐字节线序必须换自建传输层，本期不做。
- **不猜改线上行为**：本次只收敛已在用的头，取值逐字不变——`school_extra` → `mp`，
  `growth_extra` 与猫猫领奖的 web 降级 → `campaign_portal`，`upstream::common_headers` → `cli`。
  领奖那张表的头名从 `Origin`/`Referer` 改成小写，是 `HashMap` 键的字节差异，
  上线时 `HeaderName` 仍归一为同一个头名，线上字节不变。
- **漂移守卫**：`chat` 表与真正在发的 `account::build_chat_headers` 逐条比对（UA 除外），
  两边任一改动都会红；chat 缺版本直接报错，不拿空版本拼 UA。
- **参考基线**：`wb2api@22b7966` 的 `docs/OUTBOUND-HEADER-AUDIT.md`。该审计自己声明
  「参考表只是线索，不是验过的官方夹具」，所以本仓**不照抄**它的 43 个头与
  `x-stainless-*` / `baggage` / `X-B3-*`；只在本仓已有观测的范围内收敛。

### 7.5 TLS 指纹（本轮：量过、记账、选择 narrowing）

参考实现用一个独立桥进程换 TLS 指纹（`WB_TLS_FINGERPRINT=auto/node/python`），
`node` 档的 ClientHello 抄自 `tools/wb-tls-bridge/main.go` 的 `officialCipherSuites` +
`officialHelloSpec`：**18 套件 / 10 扩展 / 无 ALPN / X25519+P-256+P-384 / 9 个签名算法 /
TLS1.3→TLS1.2 / 无 GREASE**（`README.md` 的口径是「18 ciphers, 10 extensions, no ALPN」）。

本仓**不再假设自己与它一致，而是量出来**：

- 数据与判定：`crates/buddy-switch-core/src/modules/tls_parity.rs`
  （`GOLDEN_*` 常量 = 抄来的 golden，`MEASURED_*` 常量 = 实测，`compare()` = 差异表，
  `DELTA_FIELDS` = 允许出现的差异清单）。
- 探针：`crates/buddy-switch-core/examples/tls_probe.rs`
  （`cargo run -p buddy-switch-core --example tls_probe` → 打 local-vs-golden 整张表）。
- 抓法：在 loopback 起一个只读监听器，让 `reqwest` 去连它，抓握手首包再按 TLS 结构解析。
  **不发业务请求、不连外网**，所以离线 CI 也能跑。

**实测（2026-11-16，`reqwest 0.12.28` + `rustls 0.23.43` + `ring`）**：

| 字段 | golden（uTLS 桥） | 本仓实测 | 能否用 rustls/reqwest 表达 | 处置 |
|:--|:--|:--|:--|:--|
| 密套件 | 18 条，TLS1.3 三条在前，含 7 条 CBC + 2 条 RSA-kx | 10 条 AEAD + `SCSV(0x00ff)`，顺序 `1302,1301,1303,c02c,c02b,cca9,c030,c02f,cca8` | **不能**：rustls 只实现 AEAD（无 CBC、无 RSA-kx），reqwest 不暴露顺序 | 接受差额 |
| 扩展 | 10 个，顺序 `0000,0017,ff01,000a,000b,0023,000d,0033,002d,002b` | 11 个，集合 `0000,0005,000a,000b,000d,0010,0017,0023,002b,002d,0033`；**顺序每个连接随机** | **不能**：顺序是 rustls 0.23 的抗指纹特性（每次握手重排，实测四次四种顺序）；`renegotiation_info(0xff01)` 根本不发，且多发 `status_request(0x0005)` 与 `ALPN(0x0010)` | 接受差额 |
| 签名算法 | 9 条，含 `rsa_pkcs1_sha1(0x0201)` | 9 条，多 `rsa_pss_pss_sha256(0x0807)`、少 `0x0201`，顺序不同 | **不能**：reqwest 不暴露该列表 | 接受差额 |
| ALPN | **无** | `["http/1.1"]`（本仓未启用 `http2` 特性） | 只能靠 `use_preconfigured_tls` 清空，而那需要**直接依赖 `rustls`**（新依赖） | 接受差额 |
| 支持组 | `X25519,P-256,P-384` | **一致** | — | 已对齐 |
| key_share | 首包只带 `X25519` | **一致** | — | 已对齐 |
| 版本 | `TLS1.3,TLS1.2` | **一致** | — | 已对齐 |
| SNI / 压缩 / GREASE | 有 SNI / 只有 `null` / 无 GREASE | **一致**（无 GREASE） | — | 已对齐 |

> 两个反直觉的实测结果，值得单独记：
>
> 1. rustls **会**发 `ec_point_formats(0x000b)` ——「rustls 从不发这个扩展」的常见印象是错的；
> 2. rustls 0.23 **每个连接都重新打乱扩展顺序**（抗指纹特性）。也就是说本仓连「自己两次握手
>    的扩展顺序一致」都做不到，逐字对齐 golden 的顺序在架构上就不可能——这不是缺配置。
>
> 这正是本节要求「量，而不是读源码猜」的理由：两条结论都只能抓包得到。

**本轮决策：narrowing（明确不追求逐字节对齐），不挂 uTLS 侧车。** 理由按重要性排：

1. **表达不出来，不是没配置**：golden 里 7 条 CBC、2 条 RSA-kx 套件在 rustls 里**不存在实现**；
   扩展顺序与签名算法列表也没有配置入口。要在本仓复现 golden，唯一可行路径是
   **自建 TLS 栈或引入 uTLS**（Go/Node 外部运行时），不是「调一个参数」。
2. **收益不可测**：§1 第 1 条已经写明不承诺规避封禁。当前没有任何证据表明平台的限流
   按 JA3/JA4 聚类；为一条未经验证的假设引入外部运行时（多一个进程、多一套供应链、
   多一处凭据面）不划算。相关的判断依据只能来自 §9 的两账号 spike。
3. **已对齐的部分是真的对齐**：支持组、key_share、版本、SNI、无 GREASE 五条实测一致——
   也就是说「最像机器人的那几条」没有偏差，差额集中在老套件与扩展顺序上。

**不做的后果，写在明面上**：如果平台真的按 TLS 指纹聚类，本仓请求仍与官方客户端不同；
本节不假装已对齐，也不把「差异表已记录」说成「风险已消除」。真要做时的触发条件与代价：

- 触发：§9 的两账号 spike 出现**只能用 TLS 指纹解释**的限流/差异（同期头部、出口、间隔都排除了）。
- 代价：`use_preconfigured_tls` + 直接依赖 `rustls` 只能解决 ALPN 与套件顺序的一部分；
  完整 golden 需要 uTLS/自建 ClientHello，即**引入新依赖或外部运行时 ⇒ 先单独批准**（§10 末条）。
  批准后的落点是新增一个「TLS 档」开关（与 `device_identity` / `egress_slot` 同级、默认关），
  而不是替换默认栈。

**漂移守卫**（依赖升级时不靠人记）：

- `cargo test -p buddy-switch-core --lib modules::tls_parity` —— 抓包结果必须与
  `MEASURED_*` 一致；差异字段必须正好是 `DELTA_FIELDS` 那四条；扩展顺序必须仍然随机
  （`extension_order_is_randomized_per_connection`：6 次抓包至少两种顺序）；
- `decision_is_recorded_in_the_spec` 用 `include_str!` 把本节读进编译产物：
  代码改结论而本节没改（或本节被删）就会红。

## 8. 秘密与日志

- **绝不进日志 / 台账 / 错误串**：access token、cookie、代理凭据、设备派生种子、完整账号 UID。
- 错误串里的账号只留 `account_id`（内部 id，不是 UID）；URL 里的凭据按 §7.2 打码。
- 必须有 **redaction 单测**：构造带 token/cookie/代理凭据的失败响应与配置，
  断言日志行、台账 `last_error`、`TaskIo` 错误类型里都不出现这些值（含 URL 编码形式）。

## 9. 验收与测试

**离线（CI 必跑）**

1. `plan()` 的纯函数测试：27 条 `TASK_SPECS` + 7 条小程序 + 两张活动表 → 期望 `Vec<Action>`；
   覆盖 `unforgeable`、桌面专属、`first_buddy` 前置、夜间窗口、当日已领奖。
   接取段的判定已落地：`task_engine` 的 `plan_accept`（§3.6）。
2. 台账测试：同一天重跑不重复上报；`attempts` 上限生效；跨日重置；`atomic_write` 失败回滚。
3. verify-before-claim 测试：进度未达 → 不发 `Claim`；复读涨到 `target` → 发一次 `Claim`。
4. 风控准入测试：`risk_control` 关闭时不产生额外请求；暂停/冷却时当轮 0 写请求。
5. redaction 测试（§8）。
6. 前端：开关默认全关、总闸与分项联动、文案里不出现「防风控 / 避免封号」类承诺（快照断言）。

**在线（人工，两枚 CN 账号）**

7. 单账号开 `tasks`，跑一轮，核对台账 `claimed` 与官方成长中心页面一致；`needs_human` 三条不被上报。
8. 开 `egress_slot` → 关 → 再开：每次下一轮的首个请求立即跟随新出口（用 `client_geo` 探测值佐证）。

## 10. 分期

本期分 20 步（见执行计划），顺序是：**决策与文档（本文 + D58–D60 + 两处 policy 修订）→ 内核与台账
→ TaskIo + 风控准入 → 调度与开关 → UI → 参考实现逐族迁移（growth / mp / campaign / buddy_box）
→ 反滥用前置（identity / egress / geo）→ 离线测试 → 两账号 spike**。

任何一步若要求引入新依赖或新系统组件（TLS 桥、外部浏览器自动化），**停下来先问**。
