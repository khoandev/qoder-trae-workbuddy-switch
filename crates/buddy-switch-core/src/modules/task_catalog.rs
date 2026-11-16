//! WorkBuddy 任务目录：**唯一一份**任务元数据（kind / target / reward / 旗标 / 专家对）。
//!
//! 依据：`docs/specs/WB-AUTOMATION-ARCH.md` §3.4；参考基线 `wb2api@22b7966` 的
//! `wb_tasks.py`（`MP_TASK_SPECS:52`、`CAMPAIGN_SPECS:74`、`TASK_SPECS:137`、
//! `DESKTOP_ONLY_TASKS:219`、`NIGHT_TASK_CODES:225`、`EXPERT_ID_POOL:234`、`TEAM_ID_POOL:246`、
//! 动态专家 `extract_expert_from_task:484`）。
//!
//! 四句话：
//!
//! 1. **纯数据 + 纯函数**：本模块没有 IO、不读时钟、不碰账号对象（除了 [`gate_for`] 读一个字段）。
//!    任务链的「做什么」全在这里，「怎么发」在 `task_io` / `wb_tasks/event`。
//! 2. **服务端优先，目录兜底**：名称 / `target` / `reward` 的服务端值覆盖目录值；目录只提供
//!    缺省与**语义旗标**（`unforgeable` / `desktop_only` / `night_only`）。这条与参考实现
//!    `fetch_growth_tasks` 的 `title or spec.name` / `target or spec.target` 完全一致。
//! 3. **未知任务码 = 明确跳过**：参考实现对没有 spec 也没有 `expert_use_` 前缀的任务码
//!    不发任何事件（返回 0）。这里把「未知」做成 [`SpecSource::Unknown`] 而不是猜一个 kind，
//!    否则新任务码会被塞进错误的判定器、上报无效事件。
//! 4. **不猜企业号与海外区**：`is_enterprise` 或 `region != Cn` 一律 [`Gate::Skip`]，
//!    与参考实现 `if account.is_enterprise(): return []` / `account.realm != "cn"` 同判据。
//!
//! 目录里**没有也不许有**任何凭据：只放 task code、中文名、专家 id、加油位。测试
//! `catalog_holds_no_credentials` 守着这条。

use std::time::Duration;

use serde_json::Value;

use crate::modules::account;
use crate::modules::region::Region;

/// 任务判定器类型（决定用哪套事件构造器）。**不含**「不可伪造」类：那些任务没有事件链。
///
/// `as_str()` 与参考实现的 `kind` 字面量逐字一致，便于对账与台账。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TaskKind {
    Canvas,
    Template,
    Expert,
    Team,
    Skill,
    Automation,
    Playbook,
    Lighthouse,
    Buddy5,
    Skin,
    Chat,
    GlmChat,
    Cat,
    RichMeow,
    Library,
    SkillShare,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canvas => "canvas",
            Self::Template => "template",
            Self::Expert => "expert",
            Self::Team => "team",
            Self::Skill => "skill",
            Self::Automation => "automation",
            Self::Playbook => "playbook",
            Self::Lighthouse => "lighthouse",
            Self::Buddy5 => "buddy5",
            Self::Skin => "skin",
            Self::Chat => "chat",
            Self::GlmChat => "glmchat",
            Self::Cat => "cat",
            Self::RichMeow => "richmeow",
            Self::Library => "library",
            Self::SkillShare => "skill_share",
        }
    }
}

/// 一条成长任务元数据。
///
/// 旗标用 `Option<&'static str>` 而不是 `bool + String`：**「是不是」与「为什么」不会各说各话**，
/// 有理由就一定有旗标（[`Self::desktop_only`] / [`Self::unforgeable`] 是派生访问器）。
#[derive(Clone, Copy, Debug)]
pub struct TaskSpec {
    pub code: &'static str,
    /// 中文名（服务端 `title` 缺失时用它）。
    pub name: &'static str,
    /// 判定器。`None` = 没有事件链（不可伪造类），只能标 `needs_human`。
    pub kind: Option<TaskKind>,
    pub target: i64,
    pub reward: i64,
    /// 固定专家 `(id, name)`。动态 `expert_use_*` 由 [`resolve`] 现场推。
    pub expert: Option<(&'static str, &'static str)>,
    /// `Some(提示)` = 需真实桌面操作，伪造事件无效。
    pub desktop_only: Option<&'static str>,
    /// `Some(原因)` = 需真人动作，一律 `needs_human`。
    pub unforgeable: Option<&'static str>,
    /// 只在 CST 23:00–08:00 计数（窗口判定在 `cst::in_night_window`，不在这里重复）。
    pub night_only: bool,
}

impl TaskSpec {
    pub fn is_desktop_only(&self) -> bool {
        self.desktop_only.is_some()
    }

    pub fn is_unforgeable(&self) -> bool {
        self.unforgeable.is_some()
    }

    /// 需要真人/桌面才能推进时的理由（优先「不可伪造」，因为没有替代路径）。
    pub fn needs_human_reason(&self) -> Option<&'static str> {
        self.unforgeable.or(self.desktop_only)
    }
}

const NO: Option<&'static str> = None;

/// 成长任务目录（27 条，顺序照抄 `TASK_SPECS` 的声明顺序，方便逐条对账）。
///
/// `Expert_Philanthropy` / `wb_wechat_oa_subscribe_task` 的 `kind` 是 `None`——参考实现里
/// 它们也没有 `kind`，只有 `unforgeable + reason`：**没有可上报的事件链**，这是刻意的。
pub const GROWTH_TASKS: [TaskSpec; 27] = [
    TaskSpec {
        code: "create_canvas",
        name: "创建设计任务",
        kind: Some(TaskKind::Canvas),
        target: 1,
        reward: 300,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "template_5",
        name: "模板创建任务",
        kind: Some(TaskKind::Template),
        target: 5,
        reward: 200,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "expert_5",
        name: "使用专家助手",
        kind: Some(TaskKind::Expert),
        target: 5,
        reward: 200,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Expert_team_use_3",
        name: "使用专家团队",
        kind: Some(TaskKind::Team),
        target: 3,
        reward: 150,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "skill_1",
        name: "体验技能",
        kind: Some(TaskKind::Skill),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "automation_1",
        name: "创建自动化任务",
        kind: Some(TaskKind::Automation),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "playbook_prompt",
        name: "灵感案例使用",
        kind: Some(TaskKind::Playbook),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Expert_lighthouse",
        name: "轻量云专家使用",
        kind: Some(TaskKind::Lighthouse),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Buddy_App",
        name: "进入 Buddy 应用",
        kind: Some(TaskKind::Buddy5),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: Some("在桌面端左上角「发现应用」进入任意一个 Buddy 应用"),
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Buddy_App_QQ",
        name: "企鹅教师助手",
        kind: Some(TaskKind::Buddy5),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: Some("在桌面端「发现应用」进入「企鹅教师助手」"),
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Hp_Appearance",
        name: "应用主题外观",
        kind: Some(TaskKind::Skin),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "chat_5",
        name: "发起 5 次对话",
        kind: Some(TaskKind::Chat),
        target: 5,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Model_chat_GLM5.2",
        name: "体验 GLM-5.2",
        kind: Some(TaskKind::GlmChat),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "black_cat",
        name: "夜猫子任务 (23:00-08:00)",
        kind: Some(TaskKind::Cat),
        target: 3,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: true,
    },
    TaskSpec {
        code: "RichMeow_Chat",
        name: "桌面对话事件链",
        kind: Some(TaskKind::RichMeow),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Library_read",
        name: "浏览资料库",
        kind: Some(TaskKind::Library),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "first_buddy",
        name: "领取一只 Buddy",
        kind: Some(TaskKind::Chat),
        target: 1,
        reward: 300,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "skill_share",
        name: "分享技能给好友",
        kind: Some(TaskKind::SkillShare),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "WorkBuddy_Valorant",
        name: "体验「无畏契约拼豆专家」",
        kind: Some(TaskKind::Expert),
        target: 1,
        reward: 100,
        expert: Some(("ex_xQHHD1u5Jkua", "无畏契约拼豆专家")),
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "workstation_expert",
        name: "工作台搭建师",
        kind: Some(TaskKind::Expert),
        target: 1,
        reward: 100,
        expert: Some(("WorkspaceBuilder", "工作台搭建师")),
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "expert_use_maidehao",
        name: "中国电商运营专家",
        kind: Some(TaskKind::Expert),
        target: 1,
        reward: 100,
        expert: Some(("ex_GDl31WahyIH0", "中国电商运营专家")),
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "expert_use_douyinyin",
        name: "抖音策略师",
        kind: Some(TaskKind::Expert),
        target: 1,
        reward: 100,
        expert: Some(("ex_FbY5nlWTqDod", "抖音策略师")),
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "expert_use_shangjia_xuanchuan",
        name: "商家宣传页与海报设计师",
        kind: Some(TaskKind::Expert),
        target: 1,
        reward: 100,
        expert: Some(("ex_Weo3x2EZuSDm", "商家宣传页与海报设计师")),
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "expert_use_bowangwang",
        name: "直播电商教练",
        kind: Some(TaskKind::Expert),
        target: 1,
        reward: 100,
        expert: Some(("ex_WZY136wV5aMt", "直播电商教练")),
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    TaskSpec {
        code: "Sequential_Tasks_7",
        name: "PC灵感功能",
        kind: Some(TaskKind::Playbook),
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: NO,
        night_only: false,
    },
    // 参考实现这两条只有 `name` / `reward` / `unforgeable`+`reason`，**没有** `kind` 与 `target`；
    // 这里的 `target: 1` 只是 `_normalize_task` 的缺省值（它们从不进入进度判定）。
    TaskSpec {
        code: "wb_wechat_oa_subscribe_task",
        name: "关注官方公众号",
        kind: None,
        target: 1,
        reward: 100,
        expert: None,
        desktop_only: NO,
        unforgeable: Some("需在微信关注「腾讯 WorkBuddy」公众号满 24 小时"),
        night_only: false,
    },
    TaskSpec {
        code: "Expert_Philanthropy",
        name: "公益爱心捐赠",
        kind: None,
        target: 1,
        reward: 0,
        expert: None,
        desktop_only: NO,
        unforgeable: Some("真实捐款动作"),
        night_only: false,
    },
];

/// 小程序任务元数据（`MP_TASK_SPECS`，7 条）。
#[derive(Clone, Copy, Debug)]
pub struct MpTaskSpec {
    pub code: &'static str,
    pub name: &'static str,
    pub kind: TaskKind,
    pub target: i64,
    pub reward: i64,
}

/// 小程序任务目录（`Sequential_Tasks_1..6` + `school_season`）。
///
/// `kind` 来自参考实现的逐码事件构造分支：1/3/6 与 `school_season` 是对话，
/// 2 是「工作台搭建师」专家，4 是定时任务，5 是 GLM-5.2 对话。
pub const MP_TASKS: [MpTaskSpec; 7] = [
    MpTaskSpec {
        code: "Sequential_Tasks_1",
        name: "小程序对话",
        kind: TaskKind::Chat,
        target: 1,
        reward: 100,
    },
    MpTaskSpec {
        code: "Sequential_Tasks_2",
        name: "小程序专家对话",
        kind: TaskKind::Expert,
        target: 1,
        reward: 200,
    },
    MpTaskSpec {
        code: "Sequential_Tasks_3",
        name: "小程序对话5次",
        kind: TaskKind::Chat,
        target: 5,
        reward: 300,
    },
    MpTaskSpec {
        code: "Sequential_Tasks_4",
        name: "小程序定时任务",
        kind: TaskKind::Automation,
        target: 1,
        reward: 100,
    },
    MpTaskSpec {
        code: "Sequential_Tasks_5",
        name: "小程序GLM5.2",
        kind: TaskKind::GlmChat,
        target: 1,
        reward: 100,
    },
    MpTaskSpec {
        code: "Sequential_Tasks_6",
        name: "小程序对话10次",
        kind: TaskKind::Chat,
        target: 10,
        reward: 400,
    },
    MpTaskSpec {
        code: "school_season",
        name: "校园日活动",
        kind: TaskKind::Chat,
        target: 1,
        reward: 100,
    },
];

/// 小程序过滤用的前缀：参考实现放行 `Sequential_Tasks_*`，但**只有** [`MP_TASKS`] 里那几条
/// 有事件构造器——其余前缀码解析为 [`SpecSource::Unknown`]（跳过），不猜事件。
pub const MP_SEQUENTIAL_PREFIX: &str = "Sequential_Tasks_";

/// 小程序专家所属分类（`MP_EXPERT_CATEGORY`）。
pub const MP_EXPERT_CATEGORY: &str = "16-BackToSchool";

// ---------------------------------------------------------------------------
// 运营活动（独立 `/portal/activity/*` 契约，不在成长任务清单里）
// ---------------------------------------------------------------------------

pub const CAMPAIGN_BASE: &str = "https://www.codebuddy.cn";
/// 批内间隔。
pub const CAMPAIGN_GAP: Duration = Duration::from_secs(3);
/// 普通任务轮询。
pub const CAMPAIGN_POLL: Duration = Duration::from_secs(6);
/// 慢档（桌面等待类）间隔。
pub const CAMPAIGN_SLOW_GAP: Duration = Duration::from_secs(10);
/// 慢档轮询。
pub const CAMPAIGN_SLOW_POLL: Duration = Duration::from_secs(6);
/// 领奖间隔。
pub const CAMPAIGN_CLAIM_GAP: Duration = Duration::from_secs(4);
/// 领奖轮询。
pub const CAMPAIGN_CLAIM_POLL: Duration = Duration::from_secs(5);
/// 桌面动作的最长等待。
pub const CAMPAIGN_DESKTOP_WAIT: Duration = Duration::from_secs(60);
/// 桌面动作轮询。
pub const CAMPAIGN_DESKTOP_POLL: Duration = Duration::from_secs(6);

/// 活动子任务的判定器。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CampaignTaskKind {
    Chat,
    Expert,
    Skill,
    /// 需桌面端真实打开灵感案例。
    Desktop,
    /// 需桌面端进入 App。
    App,
}

impl CampaignTaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Expert => "expert",
            Self::Skill => "skill",
            Self::Desktop => "desktop",
            Self::App => "app",
        }
    }
}

/// 一条活动子任务。
#[derive(Clone, Copy, Debug)]
pub struct CampaignTask {
    /// 上游 `task` 标识（活动契约里用它，不是成长任务的 `task_code`）。
    pub task: &'static str,
    pub label: &'static str,
    pub kind: CampaignTaskKind,
    /// 需要点亮的次数（缺省 1）。
    pub times: i64,
    pub credit: i64,
    pub expert: Option<(&'static str, &'static str)>,
    pub skill: Option<(&'static str, &'static str)>,
    pub case_id: Option<&'static str>,
}

/// 一个运营活动。
#[derive(Clone, Copy, Debug)]
pub struct CampaignSpec {
    pub code: &'static str,
    pub label: &'static str,
    pub page_url: &'static str,
    pub page_name: &'static str,
    pub tasks: &'static [CampaignTask],
}

const STARBUCKS_TASKS: [CampaignTask; 3] = [
    CampaignTask {
        task: "ai_chat",
        label: "与 AI 对话3次",
        kind: CampaignTaskKind::Chat,
        times: 3,
        credit: 100,
        expert: None,
        skill: None,
        case_id: None,
    },
    CampaignTask {
        task: "expert",
        label: "召唤专家并对话",
        kind: CampaignTaskKind::Expert,
        times: 1,
        credit: 100,
        expert: None,
        skill: None,
        case_id: None,
    },
    CampaignTask {
        task: "app",
        label: "解锁 App 强大能力",
        kind: CampaignTaskKind::App,
        times: 1,
        credit: 100,
        expert: None,
        skill: None,
        case_id: None,
    },
];

const REPORTER_TASKS: [CampaignTask; 3] = [
    CampaignTask {
        task: "expert_chat",
        label: "体验「媒体工作助理」专家",
        kind: CampaignTaskKind::Expert,
        times: 1,
        credit: 50,
        expert: Some(("ex_NaiDbO06Adf3", "媒体工作助理")),
        skill: None,
        case_id: None,
    },
    CampaignTask {
        task: "skill_use",
        label: "体验「采访素材整理」技能",
        kind: CampaignTaskKind::Skill,
        times: 1,
        credit: 50,
        expert: None,
        skill: Some(("skill_2117491159283752960", "interview-material-organizer")),
        case_id: None,
    },
    CampaignTask {
        task: "desktop_same",
        label: "体验「内容发布排期」灵感",
        kind: CampaignTaskKind::Desktop,
        times: 1,
        credit: 100,
        expert: None,
        skill: None,
        case_id: Some("content-calendar"),
    },
];

/// 活动目录（两个：星巴克协作、记者节福利）。
pub const CAMPAIGNS: [CampaignSpec; 2] = [
    CampaignSpec {
        code: "campaign_starbucks_2026",
        label: "星巴克协作",
        page_url: "subpackages/secondary/starbucks-activity-428/index",
        page_name: "starbucks_activity",
        tasks: &STARBUCKS_TASKS,
    },
    CampaignSpec {
        code: "campaign_reporter_2026",
        label: "记者节福利活动",
        page_url: "subpackages/journalist-benefit/index",
        page_name: "journalist_benefit",
        tasks: &REPORTER_TASKS,
    },
];

// ---------------------------------------------------------------------------
// 专家 / 团队 id 池
// ---------------------------------------------------------------------------

/// 专家 id 池（18 条）。**事件 id 必须互不相同**：上游按 `(eventCode, id)` 去重，
/// 重复发同一个 id 进度永远不动（参考实现 2026-09 实测）。
pub const EXPERT_POOL: [(&str, &str); 18] = [
    ("ex_PZw8Gu81HfN4", "运维工程师"),
    ("ex_ROsDtJbzADFV", "产品经理"),
    ("ex_SMUnl0nJbPix", "UI设计师"),
    ("ex_ZTR062oVBOCW", "数据分析师"),
    ("ex_a3sSSFBy8qaC", "后端架构师"),
    ("ex_aG1kvKbq8lPx", "文案策划"),
    ("ex_al1vxtUOYQ10", "测试专家"),
    ("ex_cZfiyuET9UQP", "安全顾问"),
    ("ex_eggOvQuVP0hq", "算法工程师"),
    ("ex_hSwsQjkSKnkX", "前端工程师"),
    ("ex_mMbwwmFA9n9P", "项目管理专家"),
    ("ex_uAQE5POfk7Zh", "增长运营专家"),
    ("ex_uZzSAScSy7FZ", "行业研究员"),
    ("ex_LHywGrZOtG7G", "数据分析师"),
    ("ex_NX5C8GBciVed", "测试架构师"),
    ("ex_DdCsaoq4AtcO", "云端运维专家"),
    ("ex_KzqKQguubrNQ", "内容创作专家"),
    ("ex_2cvvUZQhDyeJ", "腾讯轻量云专家"),
];

/// 专家团队 id 池（8 条）。前 3 条已在 2026-09 实测可推进 `Expert_team_use_3`。
pub const TEAM_POOL: [(&str, &str); 8] = [
    ("CloudOpsTeam", "运维专家团队"),
    ("CloudContentTeam", "内容专家团队"),
    ("CloudDevTeam", "研发专家团队"),
    ("ProductStrategyTeam", "产品战略团队"),
    ("MarketingCampaignTeam", "营销活动团队"),
    ("SalesBattleTeam", "销售作战团队"),
    ("DesignEngineTeam", "设计引擎团队"),
    ("HrOperationsTeam", "人力运营团队"),
];

fn wrap(index: i64, len: usize) -> usize {
    let len = len as i64;
    (((index % len) + len) % len) as usize
}

/// 取第 `index` 个专家（超出长度**回绕**，负数也安全）。
pub fn expert_at(index: i64) -> (&'static str, &'static str) {
    EXPERT_POOL[wrap(index, EXPERT_POOL.len())]
}

/// 取第 `index` 个团队（同上）。
pub fn team_at(index: i64) -> (&'static str, &'static str) {
    TEAM_POOL[wrap(index, TEAM_POOL.len())]
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

/// 按 code 精确查成长任务（大小写敏感，不做前缀匹配）。
pub fn growth_spec(code: &str) -> Option<&'static TaskSpec> {
    GROWTH_TASKS.iter().find(|spec| spec.code == code)
}

/// 按 code 精确查小程序任务。
pub fn mp_spec(code: &str) -> Option<&'static MpTaskSpec> {
    MP_TASKS.iter().find(|spec| spec.code == code)
}

/// 按 code 精确查活动。
pub fn campaign_spec(code: &str) -> Option<&'static CampaignSpec> {
    CAMPAIGNS.iter().find(|spec| spec.code == code)
}

/// 在活动内按 `task` 查子任务。
pub fn campaign_task(campaign: &CampaignSpec, task: &str) -> Option<&'static CampaignTask> {
    campaign.tasks.iter().find(|t| t.task == task)
}

// ---------------------------------------------------------------------------
// 解析（服务端字段优先，目录兜底）
// ---------------------------------------------------------------------------

/// 解析结果的来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecSource {
    /// 命中静态目录。
    Catalog,
    /// 命中 `expert_use_*` / `jump_url` 里的专家 id（现场推导）。
    DynamicExpert,
    /// 目录没有、也推不出：**明确跳过**，不猜 kind。
    Unknown,
}

impl SpecSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::DynamicExpert => "dynamic_expert",
            Self::Unknown => "unknown",
        }
    }
}

/// 服务端任务清单里的一条原始任务（只取目录决策需要的字段）。
#[derive(Clone, Copy, Debug, Default)]
pub struct RawTask<'a> {
    pub code: &'a str,
    /// 服务端 `title`（优先于目录名）。
    pub title: Option<&'a str>,
    pub jump_url: Option<&'a str>,
    /// 服务端 `progress.target`（优先于目录 target）。
    pub target: Option<i64>,
    /// 服务端 `reward_credit`（优先于目录 reward）。
    pub reward: Option<i64>,
}

/// 目录 + 服务端字段合成后的任务。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTask {
    pub code: String,
    pub name: String,
    /// `None` = 没有可上报的事件链（不可伪造类），或未知任务码。
    pub kind: Option<TaskKind>,
    pub target: i64,
    pub reward: i64,
    pub expert: Option<(String, String)>,
    pub desktop_only: Option<String>,
    pub unforgeable: Option<String>,
    pub night_only: bool,
    pub source: SpecSource,
}

impl ResolvedTask {
    /// 目录认识这个任务码？
    pub fn is_known(&self) -> bool {
        self.source != SpecSource::Unknown
    }

    /// 需要真人/桌面才能推进？
    pub fn needs_human(&self) -> bool {
        self.unforgeable.is_some() || self.desktop_only.is_some()
    }

    /// 能否自动上报事件：认得、有事件链、且不需要真人动作。
    pub fn is_actionable(&self) -> bool {
        self.is_known() && self.kind.is_some() && !self.needs_human()
    }

    /// `needs_human` 的理由（优先「不可伪造」，因为没有替代路径）。
    pub fn needs_human_reason(&self) -> Option<&str> {
        self.unforgeable
            .as_deref()
            .or(self.desktop_only.as_deref())
    }
}

/// 解析一条**成长任务**（`fetch_growth_tasks` 的判定部分）。
///
/// 顺序与参考实现一致：静态目录 → `jump_url` 里的专家 → `expert_use_` 前缀 → 未知。
/// 静态命中优先，所以 `expert_use_maidehao` 用的是目录里的 `ex_GDl31WahyIH0`，
/// 不会被前缀规则推成 `ex_maidehao`（后者是不存在的专家）。
pub fn resolve(raw: &RawTask) -> ResolvedTask {
    let spec = growth_spec(raw.code);
    let dynamic = if spec.is_none() {
        expert_from_task(raw)
    } else {
        None
    };

    let (source, kind, expert) = match (spec, &dynamic) {
        (Some(spec), _) => (
            SpecSource::Catalog,
            spec.kind,
            spec.expert.map(|(id, name)| (id.to_string(), name.to_string())),
        ),
        (None, Some((id, name))) => (
            SpecSource::DynamicExpert,
            Some(TaskKind::Expert),
            Some((id.clone(), name.clone())),
        ),
        (None, None) => (SpecSource::Unknown, None, None),
    };

    let name = raw
        .title
        .map(str::to_string)
        .or_else(|| spec.map(|s| s.name.to_string()))
        .or_else(|| dynamic.as_ref().map(|(_, name)| name.clone()))
        .unwrap_or_else(|| raw.code.to_string());

    ResolvedTask {
        code: raw.code.to_string(),
        name,
        kind,
        target: raw.target.or_else(|| spec.map(|s| s.target)).unwrap_or(1),
        reward: raw.reward.or_else(|| spec.map(|s| s.reward)).unwrap_or(0),
        expert,
        desktop_only: spec
            .and_then(|s| s.desktop_only)
            .map(str::to_string),
        unforgeable: spec.and_then(|s| s.unforgeable).map(str::to_string),
        night_only: spec.is_some_and(|s| s.night_only),
        source,
    }
}

/// 解析一条**小程序任务**。
///
/// 参考实现对小程序清单的过滤是「在 `MP_TASK_SPECS` 里 **或** 以 `Sequential_Tasks_` 开头」，
/// 但事件构造器只覆盖 [`MP_TASKS`] 那 7 条。所以前缀码若不在目录里 → [`SpecSource::Unknown`]
/// （跳过），**不**照着前缀猜一个 kind 出来。
pub fn resolve_mp(raw: &RawTask) -> ResolvedTask {
    let spec = mp_spec(raw.code);
    ResolvedTask {
        code: raw.code.to_string(),
        name: raw
            .title
            .map(str::to_string)
            .or_else(|| spec.map(|s| s.name.to_string()))
            .unwrap_or_else(|| raw.code.to_string()),
        kind: spec.map(|s| s.kind),
        target: raw.target.or_else(|| spec.map(|s| s.target)).unwrap_or(1),
        reward: raw.reward.or_else(|| spec.map(|s| s.reward)).unwrap_or(0),
        expert: None,
        desktop_only: None,
        unforgeable: None,
        night_only: false,
        source: if spec.is_some() {
            SpecSource::Catalog
        } else {
            SpecSource::Unknown
        },
    }
}

// ---------------------------------------------------------------------------
// 动态专家解析（`extract_expert_from_task:484`）
// ---------------------------------------------------------------------------

/// 合法专家 id：`^ex_[A-Za-z0-9]{6,64}$`。
pub fn is_expert_id(value: &str) -> bool {
    match value.strip_prefix("ex_") {
        Some(rest) => {
            (6..=64).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        }
        None => false,
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 百分号解码 + `+` → 空格（query 语义）。非法转义按原样保留，不丢字节。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 取 query 参数（第一个同名值，已解码）。没有 `?` 或没有该键返回 `None`。
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    for pair in query.split('&') {
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(raw_key) == key {
            return Some(percent_decode(raw_value));
        }
    }
    None
}

/// 从任务里推专家 `(id, name)`：`jump_url` 的 `deeplink` 内层 → 外层 `expertId` →
/// `expert_use_<slug>` 前缀。全部失败返回 `None`。
pub fn expert_from_task(raw: &RawTask) -> Option<(String, String)> {
    let fallback_name = raw
        .title
        .map(str::to_string)
        .unwrap_or_else(|| raw.code.to_string());

    if let Some(jump_url) = raw.jump_url.filter(|u| !u.is_empty()) {
        // 内层 deeplink 优先：官方把真正的落地参数编码在里面。
        if let Some(deeplink) = query_param(jump_url, "deeplink") {
            if let Some(eid) = query_param(&deeplink, "expertId") {
                let eid = eid.trim();
                if is_expert_id(eid) {
                    return Some((eid.to_string(), fallback_name));
                }
            }
        }
        if let Some(eid) = query_param(jump_url, "expertId") {
            let eid = eid.trim();
            if is_expert_id(eid) {
                return Some((eid.to_string(), fallback_name));
            }
        }
    }

    if let Some(slug) = raw.code.strip_prefix("expert_use_") {
        if !slug.is_empty() {
            return Some((format!("ex_{slug}"), fallback_name));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 账号级闸门（企业号 / 海外区一律跳过）
// ---------------------------------------------------------------------------

/// 该账号能不能跑成长任务链。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gate {
    /// 可以跑。
    Proceed,
    /// 非 CN：成长任务/小程序/活动都只有 CN 有。
    NotCn,
    /// 企业（团队）号：个人成长任务接口对它没有意义，一律跳过。
    Enterprise,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proceed => "proceed",
            Self::NotCn => "skip_not_cn",
            Self::Enterprise => "skip_enterprise",
        }
    }

    pub fn proceeds(self) -> bool {
        self == Self::Proceed
    }
}

/// 结账号闸门：企业号优先判定，其次 region。
///
/// 「企业号」判据与参考实现 `is_enterprise` 一致（有 `enterpriseId`）；它还有一条
/// 「JWT 里带企业 id」的旁路，本仓不做 JWT 解码，故只看账号记录里的字段——两条都满足时
/// 行为相同，旁路缺失只会让**更少**的账号被跳过（fail-open 到「跑」），不影响非企业号。
pub fn gate_for(region: Region, acc: &Value) -> Gate {
    if account::get_str(acc, "enterpriseId")
        .or_else(|| account::get_str(acc, "enterprise_id"))
        .is_some_and(|id| !id.trim().is_empty())
    {
        return Gate::Enterprise;
    }
    if region != Region::Cn {
        return Gate::NotCn;
    }
    Gate::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes() -> Vec<&'static str> {
        GROWTH_TASKS.iter().map(|s| s.code).collect()
    }

    fn resolve_code(code: &str) -> ResolvedTask {
        resolve(&RawTask {
            code,
            ..Default::default()
        })
    }

    /// 目录规模与旗标分布（对数用；多一条/少一条都要先改 spec §3.4）。
    #[test]
    fn catalog_shape_matches_the_reference() {
        assert_eq!(GROWTH_TASKS.len(), 27, "`TASK_SPECS` 是 27 条");
        assert_eq!(MP_TASKS.len(), 7, "`MP_TASK_SPECS` 是 7 条");
        assert_eq!(CAMPAIGNS.len(), 2, "两个运营活动");
        assert_eq!(EXPERT_POOL.len(), 18, "`EXPERT_ID_POOL` 是 18 条");
        assert_eq!(TEAM_POOL.len(), 8, "`TEAM_ID_POOL` 是 8 条");

        let desktop: Vec<&str> = GROWTH_TASKS
            .iter()
            .filter(|s| s.is_desktop_only())
            .map(|s| s.code)
            .collect();
        assert_eq!(desktop, ["Buddy_App", "Buddy_App_QQ"], "`DESKTOP_ONLY_TASKS` 是 2 条");

        let unforgeable: Vec<&str> = GROWTH_TASKS
            .iter()
            .filter(|s| s.is_unforgeable())
            .map(|s| s.code)
            .collect();
        assert_eq!(
            unforgeable,
            ["wb_wechat_oa_subscribe_task", "Expert_Philanthropy"]
        );

        let night: Vec<&str> = GROWTH_TASKS
            .iter()
            .filter(|s| s.night_only)
            .map(|s| s.code)
            .collect();
        assert_eq!(night, ["black_cat"], "`NIGHT_TASK_CODES` 只有 black_cat");

        // 码集合与**声明顺序**都照抄 `TASK_SPECS`：顺序是抄写对账的锚点，别重排。
        assert_eq!(
            codes(),
            [
                "create_canvas",
                "template_5",
                "expert_5",
                "Expert_team_use_3",
                "skill_1",
                "automation_1",
                "playbook_prompt",
                "Expert_lighthouse",
                "Buddy_App",
                "Buddy_App_QQ",
                "Hp_Appearance",
                "chat_5",
                "Model_chat_GLM5.2",
                "black_cat",
                "RichMeow_Chat",
                "Library_read",
                "first_buddy",
                "skill_share",
                "WorkBuddy_Valorant",
                "workstation_expert",
                "expert_use_maidehao",
                "expert_use_douyinyin",
                "expert_use_shangjia_xuanchuan",
                "expert_use_bowangwang",
                "Sequential_Tasks_7",
                "wb_wechat_oa_subscribe_task",
                "Expert_Philanthropy",
            ]
        );
        assert_eq!(
            MP_TASKS.iter().map(|s| s.code).collect::<Vec<_>>(),
            [
                "Sequential_Tasks_1",
                "Sequential_Tasks_2",
                "Sequential_Tasks_3",
                "Sequential_Tasks_4",
                "Sequential_Tasks_5",
                "Sequential_Tasks_6",
                "school_season",
            ]
        );
    }

    /// 每条目录不败的硬约束：码唯一、名字非空、target ≥ 1、reward ≥ 0、
    /// 没有事件链的必须是「不可伪造」类（否则就是漏了 kind）。
    #[test]
    fn catalog_invariants_hold() {
        let mut seen = std::collections::HashSet::new();
        for spec in &GROWTH_TASKS {
            assert!(!spec.code.is_empty(), "空任务码");
            assert!(seen.insert(spec.code), "任务码重复：{}", spec.code);
            assert!(!spec.name.is_empty(), "{} 没有名字", spec.code);
            assert!(spec.target >= 1, "{} 的 target 必须 ≥ 1", spec.code);
            assert!(spec.reward >= 0, "{} 的 reward 不能为负", spec.code);
            if spec.kind.is_none() {
                assert!(
                    spec.is_unforgeable(),
                    "{} 没有 kind 却不是不可伪造类：要么补 kind，要么补 reason",
                    spec.code
                );
            }
            if let Some((id, name)) = spec.expert {
                assert!(!id.is_empty() && !name.is_empty(), "{} 专家对不完整", spec.code);
            }
            // 旗标与理由必须同进同退（用 Option 表达就是防这个）。
            assert_eq!(spec.is_desktop_only(), spec.desktop_only.is_some());
        }
        for spec in &MP_TASKS {
            assert!(!spec.code.is_empty() && !spec.name.is_empty());
            assert!(spec.target >= 1 && spec.reward >= 0);
        }
        let mp_codes: std::collections::HashSet<_> = MP_TASKS.iter().map(|s| s.code).collect();
        assert_eq!(mp_codes.len(), MP_TASKS.len(), "小程序任务码重复");
        for pool in [&EXPERT_POOL[..], &TEAM_POOL[..]] {
            let ids: std::collections::HashSet<_> = pool.iter().map(|(id, _)| *id).collect();
            assert_eq!(ids.len(), pool.len(), "id 池里有重复 id（上游按 id 去重，会白跑）");
        }
    }

    /// 逐条点检（表驱动）：kind / target / reward / 专家 一眼能对回 `TASK_SPECS`。
    #[test]
    fn spot_check_table_matches_the_reference() {
        let rows: [(&str, Option<TaskKind>, i64, i64); 12] = [
            ("create_canvas", Some(TaskKind::Canvas), 1, 300),
            ("template_5", Some(TaskKind::Template), 5, 200),
            ("expert_5", Some(TaskKind::Expert), 5, 200),
            ("Expert_team_use_3", Some(TaskKind::Team), 3, 150),
            ("chat_5", Some(TaskKind::Chat), 5, 100),
            ("Model_chat_GLM5.2", Some(TaskKind::GlmChat), 1, 100),
            ("black_cat", Some(TaskKind::Cat), 3, 100),
            ("first_buddy", Some(TaskKind::Chat), 1, 300),
            ("skill_share", Some(TaskKind::SkillShare), 1, 100),
            ("Sequential_Tasks_7", Some(TaskKind::Playbook), 1, 100),
            ("wb_wechat_oa_subscribe_task", None, 1, 100),
            ("Expert_Philanthropy", None, 1, 0),
        ];
        for (code, kind, target, reward) in rows {
            let spec = growth_spec(code).unwrap_or_else(|| panic!("目录缺 {code}"));
            assert_eq!(spec.kind, kind, "{code} 的 kind 不符");
            assert_eq!(spec.target, target, "{code} 的 target 不符");
            assert_eq!(spec.reward, reward, "{code} 的 reward 不符");
        }

        let experts: [(&str, &str, &str); 5] = [
            ("WorkBuddy_Valorant", "ex_xQHHD1u5Jkua", "无畏契约拼豆专家"),
            ("workstation_expert", "WorkspaceBuilder", "工作台搭建师"),
            ("expert_use_maidehao", "ex_GDl31WahyIH0", "中国电商运营专家"),
            ("expert_use_douyinyin", "ex_FbY5nlWTqDod", "抖音策略师"),
            ("expert_use_bowangwang", "ex_WZY136wV5aMt", "直播电商教练"),
        ];
        for (code, id, name) in experts {
            assert_eq!(
                growth_spec(code).and_then(|s| s.expert),
                Some((id, name)),
                "{code} 的专家对不符"
            );
        }
    }

    /// 小程序表：kind 决定事件构造器，错了等于发错事件。
    #[test]
    fn mp_table_matches_the_reference() {
        let rows: [(&str, TaskKind, i64, i64); 7] = [
            ("Sequential_Tasks_1", TaskKind::Chat, 1, 100),
            ("Sequential_Tasks_2", TaskKind::Expert, 1, 200),
            ("Sequential_Tasks_3", TaskKind::Chat, 5, 300),
            ("Sequential_Tasks_4", TaskKind::Automation, 1, 100),
            ("Sequential_Tasks_5", TaskKind::GlmChat, 1, 100),
            ("Sequential_Tasks_6", TaskKind::Chat, 10, 400),
            ("school_season", TaskKind::Chat, 1, 100),
        ];
        for (code, kind, target, reward) in rows {
            let spec = mp_spec(code).unwrap_or_else(|| panic!("小程序目录缺 {code}"));
            assert_eq!(spec.kind, kind, "{code} 的 kind 不符");
            assert_eq!(spec.target, target);
            assert_eq!(spec.reward, reward);
        }
        assert_eq!(MP_EXPERT_CATEGORY, "16-BackToSchool");
    }

    #[test]
    fn lookup_is_exact_and_case_sensitive() {
        assert!(growth_spec("chat_5").is_some());
        assert!(growth_spec("CHAT_5").is_none(), "任务码大小写敏感");
        assert!(growth_spec("chat").is_none(), "不做前缀匹配");
        assert!(growth_spec("Sequential_Tasks_99").is_none(), "前缀不等于存在");
        assert!(mp_spec("Sequential_Tasks_7").is_none(), "7 号只在 PC 目录里");
        assert!(campaign_spec("campaign_starbucks_2026").is_some());
    }

    /// 未知任务码：明确跳过，**不猜 kind**，也不假装可执行。
    #[test]
    fn unknown_code_is_unknown_and_not_actionable() {
        let got = resolve_code("brand_new_task_2027");
        assert_eq!(got.source, SpecSource::Unknown);
        assert_eq!(got.kind, None);
        assert_eq!(got.name, "brand_new_task_2027", "未知任务用 code 当名字");
        assert!(!got.is_known());
        assert!(!got.is_actionable());
        assert!(!got.needs_human(), "未知不是 needs_human，是跳过");

        // 小程序：前缀过滤放行，但目录里没有 → 仍然 Unknown（不发事件）。
        let mp = resolve_mp(&RawTask {
            code: "Sequential_Tasks_99",
            ..Default::default()
        });
        assert_eq!(mp.source, SpecSource::Unknown);
        assert!(!mp.is_actionable());
    }

    /// `expert_use_<slug>` → `ex_<slug>`（目录里没有时才走这条）。
    #[test]
    fn dynamic_expert_from_code_prefix() {
        let got = resolve_code("expert_use_newstar");
        assert_eq!(got.source, SpecSource::DynamicExpert);
        assert_eq!(got.kind, Some(TaskKind::Expert));
        assert_eq!(
            got.expert,
            Some(("ex_newstar".to_string(), "expert_use_newstar".to_string()))
        );
        assert!(got.is_actionable());
    }

    /// 静态目录优先：`expert_use_maidehao` 不能被前缀规则推成不存在的 `ex_maidehao`。
    #[test]
    fn static_spec_wins_over_dynamic_prefix() {
        let got = resolve_code("expert_use_maidehao");
        assert_eq!(got.source, SpecSource::Catalog);
        assert_eq!(got.expert.as_ref().unwrap().0, "ex_GDl31WahyIH0");
        assert_eq!(got.name, "中国电商运营专家");
    }

    /// `jump_url` 里的专家：内层 `deeplink`（URL 编码）优先，其次外层 `expertId`。
    #[test]
    fn dynamic_expert_from_jump_url() {
        let deeplink = RawTask {
            code: "expert_use_unknown_one",
            jump_url: Some(
                "https://www.workbuddy.cn/activity?deeplink=https%3A%2F%2Fwww.workbuddy.cn%2Fexpert%3FexpertId%3Dex_AbCd1234",
            ),
            ..Default::default()
        };
        let got = resolve(&deeplink);
        assert_eq!(got.source, SpecSource::DynamicExpert);
        assert_eq!(got.expert.as_ref().unwrap().0, "ex_AbCd1234");

        let plain = resolve(&RawTask {
            code: "some_expert_task",
            title: Some("某专家"),
            jump_url: Some("https://www.workbuddy.cn/e?expertId=ex_Zz9Qw8Er&from=x"),
            ..Default::default()
        });
        assert_eq!(plain.source, SpecSource::DynamicExpert);
        assert_eq!(plain.expert, Some(("ex_Zz9Qw8Er".to_string(), "某专家".to_string())));
    }

    /// 非法专家 id 不许透传（上游按 id 去重，脏 id 只会白跑）：继续往后找。
    #[test]
    fn invalid_expert_ids_are_rejected() {
        for bad in ["ex_short", "ex_", "exx_AbCd1234", "ex_has-dash", "ex_空间不足"] {
            assert!(!is_expert_id(bad), "{bad} 不该被判为合法专家 id");
        }
        assert!(is_expert_id("ex_AbCd1234"));
        assert!(is_expert_id(&format!("ex_{}", "a".repeat(64))));
        assert!(!is_expert_id(&format!("ex_{}", "a".repeat(65))));

        // 外层 expertId 非法 + 码没有 expert_use_ 前缀 → 不认（而不是带着脏 id 上报）。
        let got = resolve(&RawTask {
            code: "mystery_task",
            jump_url: Some("https://x/?expertId=ex_short"),
            ..Default::default()
        });
        assert_eq!(got.source, SpecSource::Unknown);
        assert_eq!(got.expert, None);

        // 外层非法但前缀可推 → 用前缀（前缀是本地可验证的，优先级低于合法的 jump_url）。
        let by_prefix = resolve(&RawTask {
            code: "expert_use_fallback",
            jump_url: Some("https://x/?expertId=ex_short"),
            ..Default::default()
        });
        assert_eq!(
            by_prefix.expert.as_ref().unwrap().0,
            "ex_fallback",
            "非法 jump_url 不该挡住可推的前缀"
        );
    }

    /// 服务端字段优先，目录兜底（与 `fetch_growth_tasks` 同序）。
    #[test]
    fn server_fields_override_catalog_defaults() {
        let got = resolve(&RawTask {
            code: "chat_5",
            title: Some("服务端改过的名字"),
            target: Some(8),
            reward: Some(250),
            ..Default::default()
        });
        assert_eq!(got.name, "服务端改过的名字");
        assert_eq!(got.target, 8);
        assert_eq!(got.reward, 250);
        assert_eq!(got.source, SpecSource::Catalog);

        // 缺省时用目录值。
        let defaults = resolve_code("chat_5");
        assert_eq!(defaults.name, "发起 5 次对话");
        assert_eq!(defaults.target, 5);
        assert_eq!(defaults.reward, 100);
    }

    /// 旗标必须带着理由一起出来，并且挡住自动上报。
    #[test]
    fn human_only_tasks_carry_reasons_and_block_automation() {
        let oa = resolve_code("wb_wechat_oa_subscribe_task");
        assert!(oa.needs_human());
        assert_eq!(
            oa.needs_human_reason(),
            Some("需在微信关注「腾讯 WorkBuddy」公众号满 24 小时")
        );
        assert!(!oa.is_actionable(), "不可伪造类不许自动上报");
        assert_eq!(oa.kind, None);

        let app = resolve_code("Buddy_App");
        assert!(app.needs_human());
        assert!(
            app.needs_human_reason().unwrap().contains("发现应用"),
            "桌面专属要给可照做的提示：{:?}",
            app.needs_human_reason()
        );
        assert_eq!(app.kind, Some(TaskKind::Buddy5), "桌面专属仍然有判定器，只是不上报");

        let cat = resolve_code("black_cat");
        assert!(cat.night_only);
        assert!(cat.is_actionable());
    }

    /// 企业号与海外区一律跳过（成长任务 / 小程序 / 活动共用）。
    #[test]
    fn gate_skips_enterprise_and_non_cn() {
        use serde_json::json;
        assert_eq!(gate_for(Region::Cn, &json!({"uid": "u"})), Gate::Proceed);
        assert_eq!(
            gate_for(Region::Cn, &json!({"uid": "u", "enterpriseId": "e-1"})),
            Gate::Enterprise
        );
        assert_eq!(
            gate_for(Region::Cn, &json!({"uid": "u", "enterprise_id": ""})),
            Gate::Proceed,
            "空串不算企业号"
        );
        assert_eq!(gate_for(Region::Global, &json!({"uid": "u"})), Gate::NotCn);
        // 企业号优先于 region：两个条件都成立时报企业号。
        assert_eq!(
            gate_for(Region::Global, &json!({"enterpriseId": "e-1"})),
            Gate::Enterprise
        );
        assert!(!Gate::NotCn.proceeds());
        assert_eq!(Gate::Enterprise.as_str(), "skip_enterprise");
    }

    /// 活动目录与节流常量（常量错了会让活动链打得太密）。
    #[test]
    fn campaign_table_and_timings_are_pinned() {
        let starbucks = campaign_spec("campaign_starbucks_2026").expect("星巴克活动");
        assert_eq!(starbucks.label, "星巴克协作");
        assert_eq!(starbucks.tasks.len(), 3);
        assert_eq!(starbucks.page_name, "starbucks_activity");
        assert_eq!(
            campaign_task(starbucks, "ai_chat").map(|t| (t.kind, t.times, t.credit)),
            Some((CampaignTaskKind::Chat, 3, 100))
        );
        assert_eq!(
            campaign_task(starbucks, "app").map(|t| t.kind),
            Some(CampaignTaskKind::App)
        );

        let reporter = campaign_spec("campaign_reporter_2026").expect("记者节活动");
        assert_eq!(
            campaign_task(reporter, "expert_chat").and_then(|t| t.expert),
            Some(("ex_NaiDbO06Adf3", "媒体工作助理"))
        );
        assert_eq!(
            campaign_task(reporter, "skill_use").and_then(|t| t.skill),
            Some(("skill_2117491159283752960", "interview-material-organizer"))
        );
        assert_eq!(
            campaign_task(reporter, "desktop_same").and_then(|t| t.case_id),
            Some("content-calendar")
        );

        assert_eq!(CAMPAIGN_BASE, "https://www.codebuddy.cn");
        let timings = [
            CAMPAIGN_GAP.as_secs(),
            CAMPAIGN_POLL.as_secs(),
            CAMPAIGN_SLOW_GAP.as_secs(),
            CAMPAIGN_SLOW_POLL.as_secs(),
            CAMPAIGN_CLAIM_GAP.as_secs(),
            CAMPAIGN_CLAIM_POLL.as_secs(),
            CAMPAIGN_DESKTOP_WAIT.as_secs(),
            CAMPAIGN_DESKTOP_POLL.as_secs(),
        ];
        assert_eq!(timings, [3, 6, 10, 6, 4, 5, 60, 6], "节流常量漂移了");
        // 桌面等待必须显著长于普通节流，否则「等桌面动作」会退化成忙等。
        assert!(
            CAMPAIGN_DESKTOP_WAIT >= CAMPAIGN_SLOW_GAP * 6,
            "桌面等待被调得太短，会变成忙等"
        );
    }

    /// id 池：回绕安全、相邻不同（上游按 `(eventCode, id)` 去重）。
    #[test]
    fn id_pools_wrap_and_never_repeat_adjacent() {
        for spec in &GROWTH_TASKS {
            if let Some((id, _)) = spec.expert {
                if id.starts_with("ex_") {
                    assert!(is_expert_id(id), "{} 的专家 id 形状不合法：{id}", spec.code);
                }
            }
        }
        for i in 0..EXPERT_POOL.len() as i64 * 2 {
            assert_ne!(expert_at(i).0, expert_at(i + 1).0, "相邻两次必须换 id");
        }
        assert_eq!(expert_at(0), EXPERT_POOL[0]);
        assert_eq!(expert_at(EXPERT_POOL.len() as i64), EXPERT_POOL[0], "回绕");
        assert_eq!(expert_at(-1), EXPERT_POOL[EXPERT_POOL.len() - 1], "负数回绕");
        assert_eq!(team_at(0).0, "CloudOpsTeam");
        assert_eq!(team_at(8).0, "CloudOpsTeam");
        assert_eq!(team_at(-1).0, "HrOperationsTeam");
    }

    /// 目录里不许出现任何凭据形态的字符串（这里只放任务元数据）。
    #[test]
    fn catalog_holds_no_credentials() {
        let source = include_str!("task_catalog.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        for needle in ["Bearer ", "Authorization", "access_token", "Cookie", "password"] {
            assert!(
                !production.contains(needle),
                "task_catalog.rs 里出现了与凭据有关的内容：{needle}"
            );
        }
    }
}
