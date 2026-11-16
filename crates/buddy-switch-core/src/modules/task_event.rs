//! WorkBuddy 任务事件构造器：**每个 kind 一份事件体**，id 每次尝试互不相同。
//!
//! 依据：`docs/specs/WB-AUTOMATION-ARCH.md` §3.2 / §3.5；参考基线 `wb2api@22b7966` 的
//! `wb_tasks.py`（`build_event:640`、`build_desktop_chat_events:508`、`_template_events:1078`、
//! `_skill_share_channel_events:976`、`mp_base:549`、`report_mp_events:592`、
//! `_campaign_chat_event:1566`、`_campaign_expert_events:1584`、`_campaign_skill_events:1611`、
//! `_campaign_desktop_events:1637`、`CAMPAIGN_CASES:111`）。
//!
//! 四句话：
//!
//! 1. **纯函数、无 IO、不读时钟**：`now_ms` 与 `attempt` 由调用方给（通常来自每日台账），
//!    事件体因此可以在测试里逐字断言。本模块不发请求、不碰账号对象、不碰密钥。
//! 2. **上游按 `(eventCode, id)` 去重**：凡是带 `id` 的事件，`id` 里都掺了本次尝试的序号；
//!    专家/团队取自 [`task_catalog`] 的 id 池并按 attempt 轮转（参考实现默认值是固定 id，
//!    重复发同一条不会推进进度，见 `wb_tasks.py:643` 的注释）。去重键由
//!    [`idempotency_key`] 给出，台账用它判「这条已经发过」。
//! 3. **信封只有两份**：桌面 [`desktop_envelope`] 与小程序 [`mp_envelope`]。
//!    `machineId` / `sessionId` 由 `device_identity` 派生后**传进来**，本模块不自己造设备身份，
//!    也不把安装盐、token、cookie 放进事件体。
//! 4. **不猜字段**：字段名与取值逐字抄参考实现；与参考实现的差异只有两处，都在注释里写明
//!    理由（`wbx-canvas` 的 `id` 加 attempt 后缀、专家 id 按 attempt 轮转）。

use serde_json::{json, Value};
use uuid::Uuid;

use crate::modules::task_catalog::{self, CampaignSpec, TaskKind};

/// 事件上报路径（桌面与小程序同一条契约，只是信封和域名不同）。
pub const REPORT_PATH: &str = "/v2/report";

/// 桌面默认模型（`build_event` 的 `chat` 分支）。
pub const FLASH_MODEL_ID: &str = "deepseek-v4-flash";
pub const FLASH_MODEL_NAME: &str = "DeepSeek V4 Flash";
/// GLM 模型（`glmchat` / `cat` / 小程序 5）。
pub const GLM_MODEL_ID: &str = "glm-5.2";
pub const GLM_MODEL_NAME: &str = "GLM-5.2";

/// `library` 事件里被点的那个文档页（参考实现 `LIB_DOC_URL:35`）。
pub const LIB_DOC_URL: &str = "https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm";
/// `playbook` 事件里的灵感/剧本标识与名字。
pub const PLAYBOOK_EVENT_ID: &str = "worker-ledger-freedom-dashboard";
pub const PLAYBOOK_NAME: &str = "打工人小账本";
/// `skill`（桌面 pptx 技能）事件里的示例技能。
pub const SKILL_INFO_ID: &str = "skill_2096525080079265792";
pub const SKILL_INFO_NAME: &str = "pptx";
/// `skin` 事件里的外观标识。
pub const SKIN_EVENT_ID: &str = "theme-tkmw7j";
pub const SKIN_NAME: &str = "和平精英激战金秋";
/// 小程序专家任务（`Sequential_Tasks_2`）的默认专家。
pub const MP_EXPERT_ID: &str = "WorkspaceBuilder";
pub const MP_EXPERT_NAME: &str = "工作台搭建师";
/// `Expert_lighthouse` 必须用的那个专家（任务判据认它，不能换成池里的别人）。
pub const LIGHTHOUSE_EXPERT: (&str, &str) = ("ex_2cvvUZQhDyeJ", "腾讯轻量云专家");
/// 小程序信封的扩展版本（参考实现 `mp_base:549` 里写死 2.4.0）。
pub const MP_IDE_VERSION: &str = "2.4.0";

/// 事件里的账号身份。昵称照抄上游（缺省就是空串，不要编一个）。
#[derive(Clone, Copy, Debug)]
pub struct EventUser<'a> {
    pub uid: &'a str,
    pub nick: &'a str,
}

/// 一次尝试的 id 集合。
///
/// `attempt` 由调用方单调递增（台账里的尝试序号）。**重复 id 不推进进度**，所以这里
/// 宁可牺牲与参考实现逐字一致，也要保证每次尝试的 id 不同。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventIds {
    attempt: u64,
    now_ms: i64,
    conversation_id: String,
    request_id: String,
    message_id: String,
}

impl EventIds {
    /// 桌面成长任务链：`wb-<prefix>-<now>-<attempt>`。
    pub fn for_attempt(prefix: &str, now_ms: i64, attempt: u64) -> Self {
        let stem = format!("wb-{prefix}-{now_ms}-{attempt}");
        Self {
            attempt,
            now_ms,
            conversation_id: stem.clone(),
            request_id: format!("{stem}-req"),
            message_id: format!("msg-{now_ms}-{attempt}"),
        }
    }

    /// 桌面成长任务链的默认前缀（参考实现 `wb-task-`）。
    pub fn desktop(now_ms: i64, attempt: u64) -> Self {
        Self::for_attempt("task", now_ms, attempt)
    }

    /// 小程序事件链的默认前缀（参考实现 `wb-mp-`）。
    pub fn mp(now_ms: i64, attempt: u64) -> Self {
        Self::for_attempt("mp", now_ms, attempt)
    }

    /// 活动事件链：参考实现用 uuid 会话 + `%012x` 毫秒 + uuid 前 20 位作请求 id。
    ///
    /// 活动契约的判据只认「新的一次会话」，不认确定性，所以这里用随机 id；
    /// 去重键仍然是 `(eventCode, id)`。
    pub fn campaign(now_ms: i64) -> Self {
        let conversation_id = Uuid::new_v4().simple().to_string();
        let hex = Uuid::new_v4().simple().to_string();
        let request_id = format!("{:012x}{}", now_ms.max(0) as u64, &hex[..20]);
        let message_id = format!("msg-{}", &request_id[request_id.len() - 8..]);
        Self {
            attempt: 0,
            now_ms,
            conversation_id,
            request_id,
            message_id,
        }
    }

    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn message_id(&self) -> &str {
        &self.message_id
    }
}

/// 桌面信封要用的设备身份（由 `device_identity::DeviceIdentity` 派生后传入）。
///
/// 刻意把三者分开传：本模块不持有安装盐，也不可能把盐写进事件体。
#[derive(Clone, Copy, Debug)]
pub struct DesktopDevice<'a> {
    pub machine_id: &'a str,
    pub session_id: &'a str,
    pub ide_version: &'a str,
}

/// 桌面上报信封（参考实现 `wb_fingerprint.get_desktop_fingerprint:54`）。
///
/// 其余字段是客户端固定指纹常量；`machineId` / `sessionId` / `ideVersion` 必须来自真实身份，
/// 不许在这里写死。
pub fn desktop_envelope(user: &EventUser<'_>, device: &DesktopDevice<'_>, now_ms: i64) -> Value {
    json!({
        "timezone": "Asia/Shanghai",
        "reportDelay": 2000,
        "userId": user.uid,
        "username": user.nick,
        "userNickname": user.nick,
        "product": "SaaS",
        "releaseDate": 1789036585355i64,
        "commit": "5f9692923c93033111c51ad7b003eb80204a9b75",
        "ideName": "WorkBuddy",
        "ideType": "WorkBuddy",
        "ideVersion": device.ide_version,
        "machineId": device.machine_id,
        "sessionId": device.session_id,
        "extName": "workbuddy-desktop",
        "extVersion": device.ide_version,
        "os": "win32",
        "arch": "x64",
        "osVersion": "10.0.26220",
        "cpuCores": 20,
        "memorySize": 24,
        "timestamp": now_ms,
        "presentAt": now_ms,
    })
}

/// 小程序信封（参考实现 `mp_base:549`）。
///
/// `machineId` 是 `mp-<uid 前 16 位>`；空 uid 退成 `mp-guest`。这里**不用**安装盐派生的
/// 设备 id——小程序侧的事件历史就认这个形状。
pub fn mp_envelope(user: &EventUser<'_>, now_ms: i64) -> Value {
    json!({
        "timestamp": now_ms,
        "ideType": "WorkBuddy_MP",
        "ideVersion": MP_IDE_VERSION,
        "extName": "workbuddy-mp",
        "extVersion": MP_IDE_VERSION,
        "product": "SaaS",
        "ideName": "wx_app_cloud",
        "platform": "mini_program",
        "source": "mini_program",
        "os": "android",
        "osVersion": "14",
        "arch": "arm64",
        "machineId": mp_machine_id(user.uid),
        "timezone": "Asia/Shanghai",
        "userId": user.uid,
        "userNickname": user.nick,
    })
}

fn mp_machine_id(uid: &str) -> String {
    let head: String = uid.chars().take(16).collect();
    if head.is_empty() {
        "mp-guest".to_string()
    } else {
        format!("mp-{head}")
    }
}

/// 把信封逐条压到事件上（事件字段优先，和参考实现的 `dict(base); update(event)` 一致）。
///
/// `extra` 用来放页面上下文之类只在某个活动里出现的信封字段。
pub fn with_envelope(envelope: &Value, extra: Option<&Value>, events: &[Value]) -> Vec<Value> {
    let mut base = envelope.as_object().cloned().unwrap_or_default();
    if let Some(extra) = extra.and_then(Value::as_object) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    events
        .iter()
        .map(|event| {
            let mut merged = base.clone();
            if let Some(fields) = event.as_object() {
                for (key, value) in fields {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Value::Object(merged)
        })
        .collect()
}

/// 上游去重键：`(eventCode, id)`。
///
/// 没有 `id` 的事件（对话链按 `requestId` / `messageId` 分帧）返回 `None`——这些事件本来就
/// 是一次性的，硬造一个 `id` 反而会和下一次尝试撞键。
pub fn idempotency_key(event: &Value) -> Option<(String, String)> {
    let code = event.get("eventCode")?.as_str()?.to_string();
    let id = match event.get("id")? {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    };
    Some((code, id))
}

// ---------------------------------------------------------------------------
// 桌面成长任务
// ---------------------------------------------------------------------------

/// 构造一个桌面成长任务的事件链。
///
/// 返回空 vec 的两种情况：
/// - [`TaskKind::SkillShare`]：必须先真的建出分享渠道，用 [`skill_share_chain`]（`share_code`
///   是 `id`，凭空编会在上游撞键）；
/// - [`TaskKind::Buddy5`]：盲盒走 `/travel/*` 契约，没有事件链。
pub fn desktop(kind: TaskKind, user: &EventUser<'_>, ids: &EventIds) -> Vec<Value> {
    let now = ids.now_ms();
    let cid = ids.conversation_id();
    let rid = ids.request_id();
    let uid = user.uid;
    match kind {
        TaskKind::Canvas => vec![json!({
            "eventCode": "wbx_design_canvas_task_create",
            "timestamp": now,
            "reportDelay": 0,
            "conversationId": cid,
            "requestId": rid,
            "source": "summon_keyword",
            "isCustomModel": false,
            "name": "",
            "inputLength": 12,
            // 参考实现是 `wbx-canvas-{now}`：同一毫秒内两次尝试会撞 `(eventCode, id)`。
            "id": format!("wbx-canvas-{now}-{}", ids.attempt()),
            "cost": 0,
            "isSuccessful": true,
            "userId": uid,
        })],
        TaskKind::Template => vec![json!({
            "eventCode": "agent_task_created_with_template",
            "timestamp": now,
            "reportDelay": 0,
            "isCustomModel": true,
            // 参考实现用任务下标；这里用尝试序号，保证跨轮不重复。
            "id": ids.attempt().to_string(),
            "name": "幻灯片",
            "requestId": rid,
            "conversationId": cid,
            "userId": uid,
        })],
        TaskKind::Expert | TaskKind::Team | TaskKind::Lighthouse => {
            let (expert_id, expert_name) = default_expert(kind, ids.attempt());
            let expert_type = if kind == TaskKind::Team { "team" } else { "agent" };
            vec![json!({
                "eventCode": "expert_actual_use",
                "timestamp": now,
                "reportDelay": 0,
                "mode": "CLOUD",
                "id": expert_id,
                "name": expert_name,
                "expertTitle": expert_name,
                "type": "02-Engineering",
                "expertType": expert_type,
                "source": "builtin",
                "version": "1.0.2",
                "cost": 0,
                "characterCount": 12,
                "conversationId": cid,
                "requestId": rid,
                "messageId": rid,
                "requestModelId": FLASH_MODEL_ID,
                "requestModelName": FLASH_MODEL_NAME,
                "userId": uid,
            })]
        }
        TaskKind::Skill => vec![json!({
            "eventCode": "skill_info",
            "timestamp": now,
            "reportDelay": 0,
            "skillId": SKILL_INFO_ID,
            "name": SKILL_INFO_NAME,
            "userId": uid,
        })],
        TaskKind::Automation => vec![json!({
            "eventCode": "automated_task_create_suc",
            "timestamp": now,
            "reportDelay": 0,
            "name": "每周工作整理",
            "type": "cron",
            "source": "manually",
            "modelId": FLASH_MODEL_ID,
            "modelIsThinking": false,
            "conversationId": cid,
            "requestId": rid,
            "schedule": {
                "type": "recurring",
                "rrule": "FREQ=WEEKLY;BYDAY=FR;BYHOUR=9;BYMINUTE=0",
            },
            "prompt": "每周五自动整理本周工作",
            "userId": uid,
        })],
        TaskKind::Playbook => vec![
            json!({
                "eventCode": "playbook_cta_click",
                "timestamp": now,
                "reportDelay": 0,
                "id": PLAYBOOK_EVENT_ID,
                "name": PLAYBOOK_NAME,
                "type": "document",
                "categoryId": "",
                "categoryName": "",
                "source": "discover",
                "position": 0,
                "conversationId": cid,
                "requestId": rid,
                "messageId": rid,
                "userId": uid,
            }),
            json!({
                "eventCode": "playbook_prompt_send",
                "timestamp": now + 50,
                "reportDelay": 0,
                "id": PLAYBOOK_EVENT_ID,
                "name": PLAYBOOK_NAME,
                "type": "document",
                "categoryId": "",
                "categoryName": "",
                "promptLength": 30,
                "isOfficial": 1,
                "source": "discover",
                "conversationId": cid,
                "requestId": rid,
                "messageId": rid,
                "skills": "",
                "skillNames": "",
                "userId": uid,
            }),
        ],
        TaskKind::Skin => vec![json!({
            "eventCode": "appearance_skin_apply",
            "timestamp": now,
            "reportDelay": 0,
            "action": "apply",
            "source": "settings_close",
            "id": SKIN_EVENT_ID,
            "vipLevel": "free",
            "series": "craft",
            "type": "unknown",
            "name": SKIN_NAME,
            "userId": uid,
        })],
        TaskKind::Chat | TaskKind::GlmChat | TaskKind::Cat => {
            let (model_id, model_name) = if kind == TaskKind::Chat {
                (FLASH_MODEL_ID, FLASH_MODEL_NAME)
            } else {
                (GLM_MODEL_ID, GLM_MODEL_NAME)
            };
            let mode = if kind == TaskKind::Cat { "night" } else { "craft" };
            vec![json!({
                "eventCode": "chat_request_send",
                "timestamp": now,
                "reportDelay": 0,
                "mode": mode,
                "conversationId": cid,
                "requestId": rid,
                "inputLength": 12,
                "requestModelId": model_id,
                "requestModelName": model_name,
                "isPlan": false,
                "agentName": "default",
                "agentType": "conversation",
                "userId": uid,
            })]
        }
        TaskKind::Library => vec![json!({
            "eventCode": "web_element_click",
            "timestamp": now,
            "reportDelay": 0,
            "pageURL": LIB_DOC_URL,
            "elementId": "library_doc_intro_click",
            "elementName": "WorkBuddy资料库介绍",
            "os": "Win32",
            "arch": "",
            "osVersion": "10.0",
            "userAgent": "Mozilla/5.0",
            "userId": uid,
        })],
        TaskKind::RichMeow => chat_chain(user, ids, FLASH_MODEL_ID, FLASH_MODEL_NAME),
        TaskKind::SkillShare | TaskKind::Buddy5 => Vec::new(),
    }
}

/// 专家 id 的选择：灯塔固定；团队/专家按尝试序号在 id 池里轮转。
fn default_expert(kind: TaskKind, attempt: u64) -> (&'static str, &'static str) {
    match kind {
        TaskKind::Team => task_catalog::team_at(attempt as i64),
        TaskKind::Lighthouse => LIGHTHOUSE_EXPERT,
        _ => task_catalog::expert_at(attempt as i64),
    }
}

/// 6 连桌面成功对话（点亮 `RichMeow_Chat`，参考实现 `build_desktop_chat_events:508`）。
///
/// 六帧共用一条 `conversationId`，`messageId` 三态（首帧 / `-ast` / `-resp`）互不重复；
/// 同一账号连续两轮因为 `attempt` 不同，会话 id 也不会撞。
pub fn chat_chain(
    user: &EventUser<'_>,
    ids: &EventIds,
    model_id: &str,
    model_name: &str,
) -> Vec<Value> {
    let now = ids.now_ms();
    let cid = ids.conversation_id();
    let rid = ids.request_id();
    let mid = ids.message_id();
    let uid = user.uid;
    let nick = user.nick;
    vec![
        json!({
            "eventCode": "agent_task_created",
            "timestamp": now,
            "userId": uid,
            "userNickname": nick,
            "source": "LOCAL",
            "name": "working",
            "task_target": "local",
            "mode": "craft",
            "requestModelId": model_id,
            "requestModelName": model_name,
            "has_repo": false,
            "repo_type": "none",
            "workspace_type": "empty",
            "has_connector": false,
            "connector_types": [],
            "has_mention": false,
            "mention_types": [],
            "has_template": false,
            "action": "",
            "template_name": "",
            "has_expert": false,
            "expert_id": "",
            "expert_name": "",
            "expert_industry_id": "",
            "has_skill": false,
            "skill_names": [],
            "conversationId": cid,
            "messageId": mid,
            "buddyId": "",
            "buddyName": "",
        }),
        json!({
            "eventCode": "chat_message_send",
            "timestamp": now + 20,
            "userId": uid,
            "userNickname": nick,
            "messageId": format!("{mid}-ast"),
            "historyCount": 0,
            "isContextTruncated": false,
            "currentStepCount": 1,
            "traceId": rid,
            "rootRequestId": rid,
            "parentConversationId": cid,
            "agentName": "cli",
            "agentType": "main",
        }),
        json!({
            "eventCode": "chat_request_send",
            "timestamp": now + 40,
            "userId": uid,
            "userNickname": nick,
            "inputLength": 24,
            "isPlan": false,
            "isAutoExecuteTerminal": false,
            "isAutoModify": false,
            "codebaseEnable": false,
            "maxToken": 0,
            "maxSteps": 500,
            "temperature": 0,
            "maxRetries": 0,
            "mentionContexts": [],
            "knowledgeId": [],
            "knowledgeName": [],
            "codebaseId": "",
            "mentionContextCount": 0,
            "command": "",
            "recommendId": "",
            "skillId": "",
            "skillCount": 0,
            "totalCount": 0,
            "traceId": rid,
            "rootRequestId": rid,
            "parentConversationId": cid,
            "conversationId": cid,
            "messageId": mid,
            "agentName": "cli",
            "agentType": "main",
        }),
        json!({
            "eventCode": "chat_message_response",
            "timestamp": now + 60,
            "userId": uid,
            "userNickname": nick,
            "messageId": format!("{mid}-resp"),
            "conversationId": cid,
            "requestId": rid,
            "agentName": "cli",
        }),
        json!({
            "eventCode": "chat_request_response",
            "timestamp": now + 80,
            "userId": uid,
            "userNickname": nick,
            "conversationId": cid,
            "requestId": rid,
            "requestModelId": model_id,
            "requestModelName": model_name,
            "toolCallCount": 0,
            "inputToken": 6,
            "outputToken": 12,
            "duration": 50,
            "agentName": "cli",
        }),
        json!({
            "eventCode": "agent_task_completed",
            "timestamp": now + 100,
            "userId": uid,
            "userNickname": nick,
            "conversationId": cid,
            "messageId": mid,
            "status": "success",
        }),
    ]
}

/// 技能分享渠道事件（参考实现 `_skill_share_channel_events:976`）。
///
/// `share_code` 必须来自真的建渠道的响应：它是事件的 `id`，编一个就会和别人的分享撞键。
pub fn skill_share_chain(ids: &EventIds, share_code: &str) -> Vec<Value> {
    let at = ids.now_ms();
    let skill_id = SKILL_INFO_ID;
    let skill_name = SKILL_INFO_NAME;
    vec![
        json!({
            "eventCode": "skill_share_channel_action",
            "skillId": skill_id,
            "skillName": skill_name,
            "skillType": "market",
            "action": "link",
            "panelAction": "click",
            "panelType": "clipboard",
            "isSuccessful": true,
            "cost": 0,
            "id": share_code,
            "actionAt": at,
        }),
        json!({
            "eventCode": "skill_share_channel_action",
            "skillId": skill_id,
            "skillName": skill_name,
            "skillType": "market",
            "action": "link",
            "panelAction": "success",
            "panelType": "clipboard",
            "isSuccessful": true,
            "cost": 0,
            "id": share_code,
            "actionAt": at,
        }),
    ]
}

// ---------------------------------------------------------------------------
// 小程序任务
// ---------------------------------------------------------------------------

/// 一条小程序任务的事件（`report_mp_events` 的载荷，**不含**信封）。
///
/// `kind` 从 [`task_catalog::mp_spec`] 取：不认得的任务码返回 `None`，不照前缀猜。
/// 专家任务可以传自己挑到的真实专家，缺省用「工作台搭建师」。
pub fn mp(
    code: &str,
    ids: &EventIds,
    expert: Option<(&str, &str)>,
) -> Option<Value> {
    let spec = task_catalog::mp_spec(code)?;
    Some(match spec.kind {
        TaskKind::Chat => mp_chat_request(ids, None),
        TaskKind::GlmChat => mp_chat_request(ids, Some((GLM_MODEL_ID, GLM_MODEL_NAME))),
        TaskKind::Expert => {
            let (expert_id, expert_name) = expert.unwrap_or((MP_EXPERT_ID, MP_EXPERT_NAME));
            json!({
                "eventCode": "expert_actual_use",
                "id": expert_id,
                "name": expert_id,
                "expertTitle": expert_name,
                "type": "send_message",
                "characterCount": 12,
                "expertType": "agent",
                "extVersion": "2.2.8",
            })
        }
        TaskKind::Automation => json!({
            "eventCode": "automated_task_create_suc",
            "mode": "CLOUD",
            "name": "每日任务提醒",
            "source": "manually",
            "skills": "",
            "skillCount": 0,
            "scheduleType": "daily",
        }),
        _ => return None,
    })
}

fn mp_chat_request(ids: &EventIds, model: Option<(&str, &str)>) -> Value {
    let cid = ids.conversation_id();
    let rid = ids.request_id();
    let mut event = json!({
        "eventCode": "chat_request_send",
        "mode": "chat",
        "inputLength": 12,
        "isPlan": false,
        "isAutoExecuteTerminal": false,
        "isAutoModify": false,
        "codebaseEnable": false,
        "maxToken": 0,
        "maxSteps": 500,
        "temperature": 0,
        "maxRetries": 0,
        "mentionContexts": [],
        "knowledgeId": [],
        "knowledgeName": [],
        "codebaseId": "",
        "mentionContextCount": 0,
        "command": "",
        "requestId": rid,
        "traceId": rid,
        "rootRequestId": rid,
        "parentConversationId": cid,
        "conversationId": cid,
        "messageId": ids.message_id(),
        "agentName": "mp",
        "agentType": "main",
        "codebuddy.session_id": cid,
        "codebuddy.conversation_request_id": rid,
    });
    if let (Some((model_id, model_name)), Some(fields)) = (model, event.as_object_mut()) {
        fields.insert("requestModelId".to_string(), json!(model_id));
        fields.insert("requestModelName".to_string(), json!(model_name));
    }
    event
}

// ---------------------------------------------------------------------------
// 运营活动（`/portal/activity/*` 契约）
// ---------------------------------------------------------------------------

/// 活动里的一次小程序对话（参考实现 `_campaign_chat_event:1566`）。
pub fn campaign_chat(ids: &EventIds) -> Value {
    let cid = ids.conversation_id();
    let rid = ids.request_id();
    json!({
        "eventCode": "chat_request_send",
        "mode": "chat",
        "inputLength": 12,
        "isPlan": false,
        "isAutoExecuteTerminal": false,
        "isAutoModify": false,
        "codebaseEnable": false,
        "maxToken": 0,
        "maxSteps": 500,
        "temperature": 0,
        "maxRetries": 0,
        "mentionContexts": [],
        "knowledgeId": [],
        "knowledgeName": [],
        "codebaseId": "",
        "mentionContextCount": 0,
        "command": "",
        "recommendId": "",
        "skillId": "",
        "skillCount": 0,
        "totalCount": 0,
        "traceId": rid,
        "rootRequestId": rid,
        "parentConversationId": cid,
        "conversationId": cid,
        "messageId": ids.message_id(),
        "agentName": "mp",
        "agentType": "main",
        "codebuddy.session_id": cid,
        "codebuddy.conversation_request_id": rid,
    })
}

/// 活动里的「召唤专家并对话」（参考实现 `_campaign_expert_events:1584`）。
pub fn campaign_expert(ids: &EventIds, expert_id: &str, expert_name: &str) -> Vec<Value> {
    let category = task_catalog::MP_EXPERT_CATEGORY;
    let cid = ids.conversation_id();
    let rid = ids.request_id();
    vec![
        json!({
            "eventCode": "expert_summon_click",
            "id": expert_id,
            "name": expert_id,
            "expertTitle": expert_name,
            "type": category,
            "position": 0,
        }),
        json!({
            "eventCode": "expert_summoned",
            "id": expert_id,
            "name": expert_id,
            "expertTitle": expert_name,
        }),
        json!({
            "eventCode": "expert_actual_use",
            "id": expert_id,
            "name": expert_id,
            "expertTitle": expert_name,
            "type": category,
            "characterCount": 14,
            "expertType": "builtin",
        }),
        json!({
            "eventCode": "chat_request_send",
            "inputLength": 14,
            "isPlan": false,
            "isAutoExecuteTerminal": false,
            "isAutoModify": false,
            "codebaseEnable": false,
            "maxToken": 0,
            "maxSteps": 500,
            "temperature": 0,
            "maxRetries": 0,
            "mentionContexts": [],
            "knowledgeId": [],
            "knowledgeName": [],
            "codebaseId": "",
            "mentionContextCount": 0,
            "command": "",
            "recommendId": "",
            "skillId": "",
            "skillCount": 0,
            "totalCount": 0,
            "traceId": rid,
            "rootRequestId": rid,
            "parentConversationId": cid,
            "conversationId": cid,
            "messageId": ids.message_id(),
            "agentName": "mp",
            "agentType": "main",
            "expertId": expert_id,
            "expertName": expert_name,
            "codebuddy.session_id": cid,
            "codebuddy.conversation_request_id": rid,
        }),
    ]
}

/// 活动里的「体验技能」（参考实现 `_campaign_skill_events:1611`）。
pub fn campaign_skill(ids: &EventIds, skill_id: &str, skill_name: &str) -> Vec<Value> {
    let cid = ids.conversation_id();
    let rid = ids.request_id();
    vec![
        json!({
            "eventCode": "skill_info",
            "id": skill_name,
            "skillId": skill_id,
            "skillName": skill_name,
            "skillType": "market",
            "skillSource": "market",
            "position": 0,
            "conversationId": cid,
        }),
        json!({
            "eventCode": "chat_request_send",
            "inputLength": 14,
            "isPlan": false,
            "isAutoExecuteTerminal": false,
            "isAutoModify": false,
            "codebaseEnable": false,
            "maxToken": 0,
            "maxSteps": 500,
            "temperature": 0,
            "maxRetries": 0,
            "mentionContexts": [],
            "knowledgeId": [],
            "knowledgeName": [],
            "codebaseId": "",
            "mentionContextCount": 0,
            "command": "",
            "recommendId": "",
            "skillId": skill_id,
            "skillCount": 1,
            "totalCount": 0,
            "traceId": rid,
            "rootRequestId": rid,
            "parentConversationId": cid,
            "conversationId": cid,
            "messageId": ids.message_id(),
            "agentName": "mp",
            "agentType": "main",
            "codebuddy.session_id": cid,
            "codebuddy.conversation_request_id": rid,
        }),
    ]
}

/// 活动「桌面灵感」案例表的一条（参考实现 `CAMPAIGN_CASES:111`）。
///
/// 判据认**产物目录名**（`event_id`），不认注册表 slug 或中文名——所以这张表必须逐字抄。
#[derive(Clone, Copy, Debug)]
pub struct DesktopCase {
    pub case_id: &'static str,
    pub event_id: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
    pub prompt_length: i64,
}

/// 参考实现里唯一内置的桌面案例。
pub const DESKTOP_CASES: [DesktopCase; 1] = [DesktopCase {
    case_id: "content-calendar",
    event_id: "cnb-content-calendar",
    name: "内容发布排期表",
    kind: "table",
    prompt_length: 359,
}];

/// 按案例 id 查表；不认得的案例返回 `None`（调用方记 `needs_human`，别编一个案例）。
pub fn desktop_case(case_id: &str) -> Option<&'static DesktopCase> {
    DESKTOP_CASES.iter().find(|case| case.case_id == case_id)
}

/// 活动里的「桌面灵感」事件（参考实现 `_campaign_desktop_events:1637`）。
pub fn campaign_desktop(ids: &EventIds, case_id: &str) -> Option<Value> {
    let case = desktop_case(case_id)?;
    Some(json!({
        "eventCode": "playbook_prompt_send",
        "id": case.event_id,
        "name": case.name,
        "type": case.kind,
        "promptLength": case.prompt_length,
        "isOfficial": 1,
        "source": "discover",
        "query": "",
        "ext1": "discover",
        "requestId": ids.request_id(),
        "conversationId": ids.conversation_id(),
    }))
}

/// 活动事件的页面上下文（`CAMPAIGN_SPECS` 的 `pageURL` / `pageName`），压在信封上。
pub fn campaign_page(spec: &CampaignSpec) -> Value {
    json!({
        "pageURL": spec.page_url,
        "pageName": spec.page_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> EventUser<'static> {
        EventUser {
            uid: "uid-1",
            nick: "nick-1",
        }
    }

    const NOW: i64 = 1_763_000_000_000;

    /// 有事件链的桌面 kind 与它的首帧 `eventCode`、帧数。
    const DESKTOP_CASES_TABLE: [(TaskKind, &str, usize); 13] = [
        (TaskKind::Canvas, "wbx_design_canvas_task_create", 1),
        (TaskKind::Template, "agent_task_created_with_template", 1),
        (TaskKind::Expert, "expert_actual_use", 1),
        (TaskKind::Team, "expert_actual_use", 1),
        (TaskKind::Lighthouse, "expert_actual_use", 1),
        (TaskKind::Skill, "skill_info", 1),
        (TaskKind::Automation, "automated_task_create_suc", 1),
        (TaskKind::Playbook, "playbook_cta_click", 2),
        (TaskKind::Skin, "appearance_skin_apply", 1),
        (TaskKind::Chat, "chat_request_send", 1),
        (TaskKind::GlmChat, "chat_request_send", 1),
        (TaskKind::Cat, "chat_request_send", 1),
        (TaskKind::Library, "web_element_click", 1),
    ];

    #[test]
    fn desktop_events_carry_required_fields_and_expected_codes() {
        let user = user();
        for (kind, code, frames) in DESKTOP_CASES_TABLE {
            for attempt in 0..3 {
                let ids = EventIds::desktop(NOW, attempt);
                let events = desktop(kind, &user, &ids);
                assert_eq!(
                    events.len(),
                    frames,
                    "{}: unexpected frame count",
                    kind.as_str()
                );
                assert_eq!(
                    events[0]["eventCode"], code,
                    "{}: unexpected first eventCode",
                    kind.as_str()
                );
                for event in &events {
                    assert!(
                        event["eventCode"].as_str().is_some_and(|s| !s.is_empty()),
                        "{}: missing eventCode",
                        kind.as_str()
                    );
                    assert!(
                        event["timestamp"].is_i64(),
                        "{}: missing timestamp",
                        kind.as_str()
                    );
                    assert_eq!(
                        event["userId"], user.uid,
                        "{}: missing userId",
                        kind.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn rich_meow_chain_has_six_frames_with_distinct_ids() {
        let user = user();
        let ids = EventIds::desktop(NOW, 0);
        let events = desktop(TaskKind::RichMeow, &user, &ids);
        assert_eq!(events.len(), 6);
        let codes: Vec<&str> = events
            .iter()
            .map(|event| event["eventCode"].as_str().unwrap())
            .collect();
        assert_eq!(
            codes,
            [
                "agent_task_created",
                "chat_message_send",
                "chat_request_send",
                "chat_message_response",
                "chat_request_response",
                "agent_task_completed",
            ]
        );

        let mut message_ids: Vec<String> = events
            .iter()
            .filter_map(|event| event["messageId"].as_str().map(str::to_string))
            .collect();
        message_ids.sort();
        message_ids.dedup();
        assert_eq!(message_ids.len(), 3, "messageId must be mid / -ast / -resp");

        for event in &events {
            // `chat_message_send` 只带 `parentConversationId`（参考实现如此），其余帧都带
            // `conversationId`；任一都必须指向同一次尝试的会话。
            let conversation = event
                .get("conversationId")
                .or_else(|| event.get("parentConversationId"))
                .and_then(Value::as_str)
                .expect("every frame names the conversation");
            assert_eq!(conversation, ids.conversation_id());
        }
        assert_eq!(events[0]["timestamp"], NOW);
        assert_eq!(events[5]["timestamp"], NOW + 100);
    }

    #[test]
    fn attempt_scoped_ids_are_unique_and_pool_ids_rotate() {
        // canvas / template 的 id 掺了 attempt ⇒ 跨轮永不重复。
        let user = user();
        let mut keys = std::collections::BTreeSet::new();
        for attempt in 0..64 {
            let ids = EventIds::desktop(NOW, attempt);
            for kind in [TaskKind::Canvas, TaskKind::Template] {
                for event in desktop(kind, &user, &ids) {
                    let key = idempotency_key(&event).expect("event has an id");
                    keys.insert(key);
                }
            }
        }
        assert_eq!(keys.len(), 64 * 2, "attempt-scoped ids must never repeat");

        // 专家 / 团队用的是上游认识的固定 id 池（18 / 8 条），所以 id 只能保证在池子的一轮内
        // 互不相同。超出池子后重复是**上游契约**导致的：台账在该任务点亮后就该停手，
        // 而不是靠再造一个假 id 骗进度。
        let mut expert_keys = std::collections::BTreeSet::new();
        for attempt in 0..task_catalog::EXPERT_POOL.len() {
            let ids = EventIds::desktop(NOW, attempt as u64);
            for event in desktop(TaskKind::Expert, &user, &ids) {
                let key = idempotency_key(&event).expect("expert event has an id");
                expert_keys.insert(key);
            }
        }
        assert_eq!(expert_keys.len(), task_catalog::EXPERT_POOL.len());

        let mut team_keys = std::collections::BTreeSet::new();
        for attempt in 0..task_catalog::TEAM_POOL.len() {
            let ids = EventIds::desktop(NOW, attempt as u64);
            for event in desktop(TaskKind::Team, &user, &ids) {
                let key = idempotency_key(&event).expect("team event has an id");
                team_keys.insert(key);
            }
        }
        assert_eq!(team_keys.len(), task_catalog::TEAM_POOL.len());
    }

    #[test]
    fn expert_ids_rotate_with_the_attempt() {
        let user = user();
        let first = desktop(TaskKind::Expert, &user, &EventIds::desktop(NOW, 0));
        let second = desktop(TaskKind::Expert, &user, &EventIds::desktop(NOW, 1));
        assert_ne!(first[0]["id"], second[0]["id"]);

        let team_first = desktop(TaskKind::Team, &user, &EventIds::desktop(NOW, 0));
        let team_second = desktop(TaskKind::Team, &user, &EventIds::desktop(NOW, 1));
        assert_ne!(team_first[0]["id"], team_second[0]["id"]);
        assert_eq!(team_first[0]["expertType"], "team");

        let lighthouse = desktop(TaskKind::Lighthouse, &user, &EventIds::desktop(NOW, 3));
        assert_eq!(lighthouse[0]["id"], LIGHTHOUSE_EXPERT.0);
        assert_eq!(lighthouse[0]["name"], LIGHTHOUSE_EXPERT.1);
    }

    #[test]
    fn skill_share_needs_a_share_code_and_buddy_box_has_no_chain() {
        let user = user();
        let ids = EventIds::desktop(NOW, 0);
        assert!(desktop(TaskKind::SkillShare, &user, &ids).is_empty());
        assert!(desktop(TaskKind::Buddy5, &user, &ids).is_empty());

        let events = skill_share_chain(&ids, "share-abc");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["id"], "share-abc");
        assert_eq!(events[1]["panelAction"], "success");
        assert_eq!(
            idempotency_key(&events[0]).unwrap(),
            idempotency_key(&events[1]).unwrap()
        );
    }

    #[test]
    fn mp_tasks_map_to_catalog_kinds() {
        let codes = [
            ("Sequential_Tasks_1", "chat_request_send"),
            ("Sequential_Tasks_2", "expert_actual_use"),
            ("Sequential_Tasks_3", "chat_request_send"),
            ("Sequential_Tasks_4", "automated_task_create_suc"),
            ("Sequential_Tasks_5", "chat_request_send"),
            ("Sequential_Tasks_6", "chat_request_send"),
            ("school_season", "chat_request_send"),
        ];
        for (attempt, (code, expected)) in codes.into_iter().enumerate() {
            let ids = EventIds::mp(NOW, attempt as u64);
            let event = mp(code, &ids, None).expect("known mp code yields an event");
            assert_eq!(event["eventCode"], expected, "{code}");
            if expected == "chat_request_send" {
                assert_eq!(event["parentConversationId"], ids.conversation_id());
                assert_eq!(event["codebuddy.session_id"], ids.conversation_id());
                assert_eq!(event["messageId"], ids.message_id());
            }
        }

        // GLM 分支要带模型；普通对话不带。
        let glm = mp("Sequential_Tasks_5", &EventIds::mp(NOW, 0), None).unwrap();
        assert_eq!(glm["requestModelId"], GLM_MODEL_ID);
        assert_eq!(glm["requestModelName"], GLM_MODEL_NAME);
        let chat = mp("Sequential_Tasks_1", &EventIds::mp(NOW, 0), None).unwrap();
        assert!(chat.get("requestModelId").is_none());
    }

    #[test]
    fn mp_unknown_codes_do_not_get_an_event() {
        let ids = EventIds::mp(NOW, 0);
        assert!(mp("Sequential_Tasks_9", &ids, None).is_none());
        assert!(mp("school_season_2027", &ids, None).is_none());
        assert!(mp("", &ids, None).is_none());
    }

    #[test]
    fn mp_envelope_is_stable_per_account_and_never_carries_secrets() {
        let first = mp_envelope(&user(), NOW);
        let again = mp_envelope(&user(), NOW + 1_000);
        assert_eq!(first["machineId"], again["machineId"]);
        assert_eq!(first["machineId"], "mp-uid-1");
        assert_eq!(first["userId"], "uid-1");
        assert_eq!(first["userNickname"], "nick-1");
        assert_eq!(first["platform"], "mini_program");
        assert_eq!(first["ideVersion"], MP_IDE_VERSION);

        let other = mp_envelope(
            &EventUser {
                uid: "uid-2",
                nick: "nick-1",
            },
            NOW,
        );
        assert_ne!(first["machineId"], other["machineId"]);

        let empty = mp_envelope(
            &EventUser {
                uid: "",
                nick: "",
            },
            NOW,
        );
        assert_eq!(empty["machineId"], "mp-guest");

        // 长 uid 只取前 16 位，和参考实现一致。
        let long = mp_envelope(
            &EventUser {
                uid: "abcdefghijklmnopqrstuvwxyz",
                nick: "",
            },
            NOW,
        );
        assert_eq!(long["machineId"], "mp-abcdefghijklmnop");
    }

    #[test]
    fn desktop_envelope_uses_the_injected_device_ids() {
        let device = DesktopDevice {
            machine_id: "machine-abc",
            session_id: "session-abc",
            ide_version: "5.8.4",
        };
        let envelope = desktop_envelope(&user(), &device, NOW);
        assert_eq!(envelope["machineId"], "machine-abc");
        assert_eq!(envelope["sessionId"], "session-abc");
        assert_eq!(envelope["ideVersion"], "5.8.4");
        assert_eq!(envelope["extVersion"], "5.8.4");
        assert_eq!(envelope["ideName"], "WorkBuddy");
        assert_eq!(envelope["timestamp"], NOW);

        let events = desktop(TaskKind::Chat, &user(), &EventIds::desktop(NOW, 0));
        let wrapped = with_envelope(&envelope, None, &events);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0]["machineId"], "machine-abc");
        // 事件字段优先于信封字段。
        assert_eq!(wrapped[0]["timestamp"], NOW);
        assert_eq!(wrapped[0]["eventCode"], "chat_request_send");
    }

    #[test]
    fn campaign_chat_frame_is_self_consistent() {
        let ids = EventIds::campaign(NOW);
        let event = campaign_chat(&ids);
        assert_eq!(event["eventCode"], "chat_request_send");
        assert_eq!(event["conversationId"], ids.conversation_id());
        assert_eq!(event["parentConversationId"], ids.conversation_id());
        assert_eq!(event["traceId"], ids.request_id());
        assert_eq!(event["messageId"], ids.message_id());
        assert!(ids.message_id().starts_with("msg-"));
        assert_eq!(ids.request_id().len(), 32);
        assert_eq!(ids.conversation_id().len(), 32);

        let other = EventIds::campaign(NOW);
        assert_ne!(ids.conversation_id(), other.conversation_id());
    }

    #[test]
    fn campaign_expert_and_skill_frames_match_the_reference() {
        let ids = EventIds::campaign(NOW);
        let expert = campaign_expert(&ids, "ex_NaiDbO06Adf3", "媒体工作助理");
        assert_eq!(expert.len(), 4);
        let codes: Vec<&str> = expert
            .iter()
            .map(|event| event["eventCode"].as_str().unwrap())
            .collect();
        assert_eq!(
            codes,
            [
                "expert_summon_click",
                "expert_summoned",
                "expert_actual_use",
                "chat_request_send"
            ]
        );
        assert_eq!(expert[3]["expertId"], "ex_NaiDbO06Adf3");
        assert_eq!(
            expert[3]["conversationId"],
            expert[3]["parentConversationId"]
        );

        let skill = campaign_skill(&ids, "skill_2117491159283752960", "interview-material-organizer");
        assert_eq!(skill.len(), 2);
        assert_eq!(skill[0]["eventCode"], "skill_info");
        assert_eq!(skill[1]["skillCount"], 1);
        assert_eq!(skill[1]["skillId"], "skill_2117491159283752960");
    }

    #[test]
    fn campaign_desktop_case_is_registry_driven() {
        let ids = EventIds::campaign(NOW);
        let event = campaign_desktop(&ids, "content-calendar").expect("known case");
        assert_eq!(event["eventCode"], "playbook_prompt_send");
        assert_eq!(event["id"], "cnb-content-calendar");
        assert_eq!(event["name"], "内容发布排期表");
        assert_eq!(event["type"], "table");
        assert_eq!(event["promptLength"], 359);

        assert!(campaign_desktop(&ids, "no-such-case").is_none());
    }

    #[test]
    fn campaign_page_context_comes_from_the_catalog() {
        let spec = task_catalog::campaign_spec("campaign_reporter_2026").expect("spec exists");
        let page = campaign_page(spec);
        assert_eq!(page["pageURL"], "subpackages/journalist-benefit/index");
        assert_eq!(page["pageName"], "journalist_benefit");

        let envelope = mp_envelope(&user(), NOW);
        let events = campaign_expert(&EventIds::campaign(NOW), "ex_NaiDbO06Adf3", "media");
        let wrapped = with_envelope(&envelope, Some(&page), &events);
        assert_eq!(wrapped.len(), 4);
        for event in &wrapped {
            assert_eq!(event["pageName"], "journalist_benefit");
            assert!(event["eventCode"].as_str().is_some());
        }
    }

    #[test]
    fn idempotency_key_prefers_the_declared_id() {
        let event = desktop(TaskKind::Template, &user(), &EventIds::desktop(NOW, 7));
        assert_eq!(
            idempotency_key(&event[0]),
            Some((
                "agent_task_created_with_template".to_string(),
                "7".to_string()
            ))
        );
        assert_eq!(
            idempotency_key(&json!({"eventCode": "x", "id": 5})),
            Some(("x".to_string(), "5".to_string()))
        );
        assert_eq!(idempotency_key(&json!({"eventCode": "x"})), None);
        assert_eq!(idempotency_key(&json!({"id": "x"})), None);
    }

    #[test]
    fn events_never_carry_credential_fields() {
        let user = user();
        let ids = EventIds::desktop(NOW, 0);
        let mut events = Vec::new();
        for kind in [
            TaskKind::Canvas,
            TaskKind::Template,
            TaskKind::Expert,
            TaskKind::Team,
            TaskKind::Lighthouse,
            TaskKind::Skill,
            TaskKind::Automation,
            TaskKind::Playbook,
            TaskKind::Skin,
            TaskKind::Chat,
            TaskKind::GlmChat,
            TaskKind::Cat,
            TaskKind::Library,
            TaskKind::RichMeow,
        ] {
            events.extend(desktop(kind, &user, &ids));
        }
        events.extend(skill_share_chain(&ids, "share-abc"));
        events.extend(campaign_expert(&EventIds::campaign(NOW), "ex_1", "name"));
        events.extend(campaign_skill(&EventIds::campaign(NOW), "skill-1", "name"));
        events.push(campaign_chat(&EventIds::campaign(NOW)));
        events.push(desktop_envelope(
            &user,
            &DesktopDevice {
                machine_id: "machine",
                session_id: "session",
                ide_version: "5.8.4",
            },
            NOW,
        ));
        events.push(mp_envelope(&user, NOW));

        for event in events {
            let fields = event.as_object().cloned().unwrap_or_default();
            for key in fields.keys() {
                let lower = key.to_ascii_lowercase();
                // `maxToken` / `inputToken` / `outputToken` 是长度与用量，不是凭证；
                // 除此之外任何 token/cookie/密钥字段都不许出现在事件体里。
                const TOKEN_COUNTS: [&str; 3] = ["maxtoken", "inputtoken", "outputtoken"];
                if TOKEN_COUNTS.contains(&lower.as_str()) {
                    continue;
                }
                for banned in ["token", "cookie", "authorization", "password", "secret", "api_key"] {
                    assert!(
                        !lower.contains(banned),
                        "event field `{key}` must never carry credentials"
                    );
                }
            }
            let wire = event.to_string();
            assert!(!wire.contains("Bearer "), "no bearer tokens in event bodies");
        }
    }
}
