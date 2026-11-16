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
3. **默认全关**。任务自动完成、设备身份派生、账号出口、地区面板、TLS 指纹桥
   **默认都是关的**，开启粒度是**单个账号**，并且必须能**立即关掉**（下一轮不再排程，见 §5）。

## 2. 与 `wb2api@22b7966` 的对账

「本仓现状」列是 2026-11-16 的实测状态（`rg` 全仓确认，不是记忆）。

### 2.1 任务完成

| # | 能力 | wb2api 参考 | 本仓现状 | 本期落点 |
|:--|:--|:--|:--|:--|
| 1 | 任务清单 | `GET /v2/activity/growth/tasks`（桌面头）；`TASK_SPECS` 24 条（`wb_tasks.py:137`）+ 清单带 `ex_*` 时动态解析专家（`wb_tasks.py:481`） | `cat.rs:29` 只取同一端点筛 `black_cat` | 通用引擎，`feature=growth_tasks` |
| 2 | 批量接取 | `POST /v2/activity/growth/tasks/accept`，体 `{"task_codes":[…]}`，每批 20；逐批记录 `accepted` / `already_accepted` / `failed`，失败**不吞**（`wb_tasks.py:359`） | 只接 `black_cat` | 通用；失败逐条可见 |
| 3 | 事件上报 | `POST /v2/report`；上游按 `(eventCode, id)` 去重 ⇒ 专家/团队 id 必须互异（`wb_tasks.py:1210`），`EXPERT_ID_POOL` 18 条（`:234`）、`TEAM_ID_POOL` 8 条 | `activity.rs` 的 `/v2/report` 构造器只有对话/心跳类事件 | 按 `kind` 生成事件，复用 `activity.rs` 的构造器 |
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
| 17 | TLS 指纹 | `WB_TLS_FINGERPRINT=auto/node/python`（默认 auto）+ 独立 Node 桥进程；`python` 表示不起桥（`wb_upstream_pool.py:133`） | rustls 默认栈 | 本期**只保留 python 等价档**；桥不实现（§7.4） |
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
| 内核 | `crates/buddy-switch-core/src/modules/wb_tasks/`（`plan.rs` / `event.rs` / `spec.rs`） | **纯函数**：从清单 + 台账算出 `Vec<Action>`，不碰网络、不看时钟（`now` 由调用方注入） |
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
| TLS 指纹桥 | **不实现** | — | 与今日一致（rustls 默认） |
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

- 槽表是**用户自己的命名代理列表**（`name` / `url` / `enabled` / 探测结果 `ip` / `country` / `asn`），
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

### 7.3 地区面板（`client_geo`）

- 由**实际出口 IP** 探测得到地区码 / 时区 / 语言，附加到任务与活动请求头；
  无出口（直连）时用本机时区，不猜测。
- 只在 `client_geo=true` 时附加；与网关侧的 `X-Client-Platform` 等**固定头**互不覆盖。
- 探测失败 ⇒ **不附加**（fail-closed 到「没有地区头」），不用默认值冒充。

### 7.4 TLS 指纹（不在本期）

参考实现用独立 Node 桥换 TLS 指纹（`WB_TLS_FINGERPRINT=auto/node/python`）。本仓**本期不实现**：
引入桥进程等于引入一个外部运行时，收益不可测而风险面明确（§1 第 1 条）。文档记录该差异即可，
若未来要做，必须先有独立的决策条目，不在本文件里预留开关。

## 8. 秘密与日志

- **绝不进日志 / 台账 / 错误串**：access token、cookie、代理凭据、设备派生种子、完整账号 UID。
- 错误串里的账号只留 `account_id`（内部 id，不是 UID）；URL 里的凭据按 §7.2 打码。
- 必须有 **redaction 单测**：构造带 token/cookie/代理凭据的失败响应与配置，
  断言日志行、台账 `last_error`、`TaskIo` 错误类型里都不出现这些值（含 URL 编码形式）。

## 9. 验收与测试

**离线（CI 必跑）**

1. `plan()` 的纯函数测试：24 条 `TASK_SPECS` + 7 条小程序 + 两张活动表 → 期望 `Vec<Action>`；
   覆盖 `unforgeable`、桌面专属、`first_buddy` 前置、夜间窗口、当日已领奖。
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
