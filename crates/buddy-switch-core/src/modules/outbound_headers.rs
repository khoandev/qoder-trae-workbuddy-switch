//! 出站请求头：每个「客户端形态」一套**有序**的表。
//!
//! 设计依据：`docs/specs/WB-AUTOMATION-ARCH.md` §7.4（参考基线 `wb2api@22b7966` 的
//! `docs/OUTBOUND-HEADER-AUDIT.md`）。四句话：
//!
//! 1. **一套形态一份表**：`desktop`（浏览器形态 JSON）、`cli`（上游刷新 / 目录）、
//!    `mp`（微信小程序）、`campaign_portal`（活动 web 门户）、`chat`（对话 SSE）。
//!    每种表在这里**只写一次**，各模块取 [`HeaderSet::to_map`] 用，不许再各自拼字面量。
//! 2. **UA / 版本不落字面量**：桌面形态复用 `config::DEFAULT_HTTP_USER_AGENT`（官网套餐页
//!    捕获的那条），CLI 形态用 [`CLI_USER_AGENT`]，chat 形态由
//!    [`crate::modules::identity::chat_user_agent`] 按 region + 版本构造。
//!    `MP_USER_AGENT` 在官方没有版本 token，是这条规则的唯一例外（常量就在本文件）。
//! 3. **顺序是构造契约，不是线上字节序证据**：`reqwest` / `http::HeaderMap` 内部是哈希表，
//!    发出的字节顺序**不受**本表控制。所以这里的顺序用来（a）让代码里只有一处知道「谁排前面」，
//!    （b）让快照测试抓住**名字与取值的漂移**。真要做到逐字节线序，得换自建传输层，本期不做
//!    ——这与参考仓库审计里「参考表只是线索，不是本仓库验过的官方夹具」的结论一致。
//! 4. **不猜改线上行为**：本模块只做**收敛**——把已在用的头搬到一处，取值逐字不变；
//!    没有停下抓包就不新增头（`campaign_portal` / `chat` / `mp` 三张表就是今天发的那些）。
//!
//! 换行的快照格式见 [`HeaderSet::wire_snapshot`]：`name: value` 每行一条，`\n` 结尾。

use std::collections::HashMap;

use crate::modules::config::DEFAULT_HTTP_USER_AGENT;
use crate::modules::identity::{self, ChatIdentity};
use crate::modules::region::{region_spec, Region};

/// 微信小程序形态 UA（`MiniProgramEnv`）。官方这条 UA 里没有版本 token，
/// 也没有可解析的版本来源，因此按原样固化在这里——**唯一允许的字面量 UA**。
pub const MP_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14; MicroMessenger/8.0.49 WeChat/0.8.0 MiniProgramEnv/android; wkbrowser xweb)";

/// CLI 形态 UA（上游刷新 / 产品目录请求）。
///
/// ⚠️ 这里的 `CLI/2.63.2 CodeBuddy/2.63.2` 是**本仓观测到的固定形态**，不是从
/// `identity` 的版本链推出来的：那条链给的是 App 版本（`WorkBuddy/<v>`），CLI 版本只在装了
/// CLI 时才存在。要让它也走版本链，得先抓包确认这两个 token 的取值规则，见
/// `docs/specs/WB-AUTOMATION-ARCH.md` §9 的在线 spike。
pub const CLI_USER_AGENT: &str = "CLI/2.63.2 CodeBuddy/2.63.2";

/// 五种出站形态。`as_str` 是稳定标识（日志 / 台账 / 后续 UI 用它，不要用 `Debug`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Surface {
    /// 桌面浏览器形态的 JSON 端点（Chrome UA）。
    Desktop,
    /// CLI 形态（上游刷新 / 目录）。
    Cli,
    /// 微信小程序。
    Mp,
    /// 运营活动 web 门户（growth-center 那一套）。
    CampaignPortal,
    /// 对话 SSE。
    Chat,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Cli => "cli",
            Self::Mp => "mp",
            Self::CampaignPortal => "campaign_portal",
            Self::Chat => "chat",
        }
    }

    /// 全部形态，顺序固定（供文档与测试遍历）。
    pub fn all() -> [Self; 5] {
        [
            Self::Desktop,
            Self::Cli,
            Self::Mp,
            Self::CampaignPortal,
            Self::Chat,
        ]
    }

    /// 按形态产出有序表。`region` 只影响 `chat` 的 `Accept-Language` 与 UA；
    /// `base` 是站点基址（区域 `billing_base`，或活动 web 域）。
    ///
    /// `chat` 形态必须给 [`ChatIdentity`]（版本来自安装态/保存值/兜底链，不能猜），
    /// 缺了就报错而不是拿空版本拼 UA。
    pub fn table(
        self,
        region: Region,
        base: &str,
        chat_identity: Option<&ChatIdentity>,
    ) -> Result<HeaderSet, String> {
        match self {
            Self::Desktop => Ok(desktop(base)),
            Self::Cli => Ok(cli(base)),
            Self::Mp => Ok(mp(base)),
            Self::CampaignPortal => Ok(campaign_portal(base, GROWTH_CENTER_PATH)),
            Self::Chat => {
                let identity = chat_identity.ok_or_else(|| {
                    "chat surface needs a resolved chat identity (version)".to_string()
                })?;
                chat(region, base, identity)
            }
        }
    }
}

/// 活动 web 门户的默认 referer 路径（growth-center 领奖 / 进度页）。
pub const GROWTH_CENTER_PATH: &str = "/profile/growth-center";

/// 一张**有序**请求头表。
///
/// 只做三件事：保序追加、按名原位替换、转成 `HashMap` 交给 `reqwest`。
/// 头名比较是 ASCII 大小写不敏感（HTTP 语义如此），写入时保留原始大小写
/// （`mp` / `campaign_portal` 的小写头名是今天在发的形态，不能"顺手规范"）。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HeaderSet {
    pairs: Vec<(String, String)>,
}

impl HeaderSet {
    pub fn new() -> Self {
        Self { pairs: Vec::new() }
    }

    /// 追加一条。同名多条**保留**（对应 HTTP 的重复头语义）。
    pub fn push(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        self.pairs.push((name.to_string(), value.into()));
        self
    }

    /// 有则**原位**替换（保持首次出现的位置），无则追加。
    pub fn set(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        let value = value.into();
        match self
            .pairs
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.1 = value,
            None => self.pairs.push((name.to_string(), value)),
        }
        self
    }

    /// 取值（ASCII 大小写不敏感）。
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// 有序视图（`name` 保留原始大小写）。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn names(&self) -> Vec<&str> {
        self.pairs.iter().map(|(name, _)| name.as_str()).collect()
    }

    pub fn into_pairs(self) -> Vec<(String, String)> {
        self.pairs
    }

    /// 交给 `reqwest` 用。同名声明的**最后一条**生效（行为与逐条 `insert` 一致）。
    pub fn to_map(&self) -> HashMap<String, String> {
        self.pairs.iter().cloned().collect()
    }

    /// 快照：`name: value` 每行一条，末尾带 `\n`。
    ///
    /// 这是**构造顺序**的契约（见模块文档第 3 条），测试拿它防漂移。
    pub fn wire_snapshot(&self) -> String {
        let mut out = String::new();
        for (name, value) in &self.pairs {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        out
    }
}

/// 桌面浏览器形态：JSON 端点的框架头（身份头由 `account::build_auth_headers` 另给）。
///
/// UA 直接取 [`DEFAULT_HTTP_USER_AGENT`]（`config` 里那条官网套餐页捕获的 Chrome UA），
/// 本文件不再抄第二份。
pub fn desktop(base: &str) -> HeaderSet {
    let mut set = HeaderSet::new();
    set.push("Accept", "application/json, text/plain, */*")
        .push("X-Requested-With", "XMLHttpRequest")
        .push("Origin", base)
        .push("Referer", format!("{base}/"))
        .push("User-Agent", DEFAULT_HTTP_USER_AGENT);
    set
}

/// CLI 形态：上游刷新 / 产品目录（`upstream::common_headers` 的历史取值）。
pub fn cli(base: &str) -> HeaderSet {
    let mut set = HeaderSet::new();
    set.push("Accept", "application/json, text/plain, */*")
        .push("X-Requested-With", "XMLHttpRequest")
        .push("Origin", base)
        .push("Referer", format!("{base}/"))
        .push("User-Agent", CLI_USER_AGENT);
    set
}

/// 微信小程序形态（头名小写，与原实现逐字一致）。
pub fn mp(base: &str) -> HeaderSet {
    let mut set = HeaderSet::new();
    set.push("user-agent", MP_USER_AGENT)
        .push("origin", base)
        .push("referer", format!("{base}/"));
    set
}

/// 小程序的**任务接口**头（清单 / 接取 / 领奖）：参考实现 `MP_HEADERS:38`。
///
/// 与 [`mp`] 的区别：`mp` 是「事件上报 / 页面请求」的形态（`origin` + `referer`），
/// 这里是「小程序调 growth 接口」的形态（`X-Client-Platform: miniprogram`）。
/// 两者不能合并：合并后要么给接口发多余的 `origin`，要么给页面漏掉小程序标记。
pub fn mp_task() -> HeaderSet {
    let mut set = HeaderSet::new();
    set.push("X-Client-Platform", "miniprogram")
        .push("User-Agent", MP_USER_AGENT);
    set
}

/// 小程序的**事件上报**头（`/v2/report`）：参考实现 `MP_REPORT_HEADERS:43`。
///
/// `X-Client-Version` 取 [`crate::modules::task_event::MP_IDE_VERSION`]，与事件信封里的
/// `ideVersion` 同源 —— 两处写死同一个版本号，迟早在一次升级里分叉。
pub fn mp_report() -> HeaderSet {
    let mut set = HeaderSet::new();
    set.push("Content-Type", "application/json")
        .push("Accept", "application/json")
        .push("X-Client-Product", "workbuddy-mp")
        .push("X-Client-Version", crate::modules::task_event::MP_IDE_VERSION)
        .push("X-Client-Platform", "mp-weixin")
        .push("X-Platform", "wechatmp");
    set
}

/// 活动 web 门户形态：`x-client-platform: web` + `Origin` / `Referer`（头名小写）。
///
/// `referer_path` 必须是**以 `/` 开头的路径**（如 [`GROWTH_CENTER_PATH`]）；
/// 拼出来的 referer 会带 `base` 前缀，避免把 `base//profile` 这种双斜杠发出去。
pub fn campaign_portal(origin: &str, referer_path: &str) -> HeaderSet {
    let path = if referer_path.starts_with('/') {
        referer_path.to_string()
    } else {
        format!("/{referer_path}")
    };
    let mut set = HeaderSet::new();
    set.push("x-client-platform", "web")
        .push("origin", origin)
        .push("referer", format!("{origin}{path}"));
    set
}

/// 对话 SSE 形态的框架头。身份头（`Authorization` / `X-User-Id` / `X-No-*` / `X-Product`）
/// 仍由 `account::build_chat_headers` 给，这里只管**形态**（含 UA）。
///
/// UA 必须由版本链构造：版本缺失或非法就报错，不拿空串或常量顶替。
pub fn chat(region: Region, base: &str, identity: &ChatIdentity) -> Result<HeaderSet, String> {
    let accept_language = match region {
        Region::Global => "en-US",
        Region::Cn => "zh-CN",
    };
    let user_agent = identity::chat_user_agent(
        region,
        &identity.client_version,
        identity.cli_version.as_deref(),
    )?;
    let mut set = HeaderSet::new();
    set.push("Accept", "application/json, text/event-stream")
        .push("Accept-Language", accept_language)
        .push("X-CodeBuddy-Request", "1")
        .push("X-Agent-Purpose", "conversation")
        .push("X-Requested-With", "XMLHttpRequest")
        .push("Origin", base)
        .push("Referer", format!("{base}/"))
        .push("Content-Type", "application/json")
        .push("User-Agent", user_agent);
    Ok(set)
}

/// 便捷包装：按 region 解析 chat 身份后再出表（离线测试也用它，避免各处抄版本）。
pub fn chat_for_region(region: Region, base: &str) -> Result<HeaderSet, String> {
    let identity = identity::resolve_chat_identity(region);
    chat(region, base, &identity)
}

/// 某 region 的区域基址（`billing_base`）。集中一处，免得各模块自己 `region_spec`。
pub fn region_base(region: Region) -> &'static str {
    region_spec(region).billing_base
}

#[cfg(test)]
mod tests {
    use super::*;

    const CN_BASE: &str = "https://www.workbuddy.cn";

    fn fixed_identity() -> ChatIdentity {
        ChatIdentity {
            client_version: "5.5.6".to_string(),
            cli_version: None,
        }
    }

    #[test]
    fn surface_labels_are_stable() {
        assert_eq!(
            Surface::all().map(Surface::as_str),
            ["desktop", "cli", "mp", "campaign_portal", "chat"]
        );
    }

    #[test]
    fn desktop_wire_order_is_pinned() {
        let set = desktop(CN_BASE);
        assert_eq!(
            set.wire_snapshot(),
            "Accept: application/json, text/plain, */*\n\
             X-Requested-With: XMLHttpRequest\n\
             Origin: https://www.workbuddy.cn\n\
             Referer: https://www.workbuddy.cn/\n\
             User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36\n"
        );
    }

    #[test]
    fn cli_wire_order_is_pinned() {
        let set = cli(CN_BASE);
        let snapshot = set.wire_snapshot();
        assert_eq!(
            snapshot,
            format!(
                "Accept: application/json, text/plain, */*\n\
                 X-Requested-With: XMLHttpRequest\n\
                 Origin: {CN_BASE}\n\
                 Referer: {CN_BASE}/\n\
                 User-Agent: {CLI_USER_AGENT}\n"
            )
        );
        // 与 desktop 只差最后一条的取值，名字与顺序必须一致：两套形态不许各自演化。
        assert_eq!(set.names(), desktop(CN_BASE).names());
    }

    #[test]
    fn mp_wire_order_is_pinned() {
        let set = mp(CN_BASE);
        assert_eq!(
            set.wire_snapshot(),
            "user-agent: Mozilla/5.0 (Linux; Android 14; MicroMessenger/8.0.49 WeChat/0.8.0 MiniProgramEnv/android; wkbrowser xweb)\n\
             origin: https://www.workbuddy.cn\n\
             referer: https://www.workbuddy.cn/\n"
        );
        assert_eq!(set.get("User-Agent"), Some(MP_USER_AGENT));
    }

    #[test]
    fn campaign_portal_uses_the_growth_center_referer() {
        let set = campaign_portal(CN_BASE, GROWTH_CENTER_PATH);
        assert_eq!(
            set.wire_snapshot(),
            "x-client-platform: web\n\
             origin: https://www.workbuddy.cn\n\
             referer: https://www.workbuddy.cn/profile/growth-center\n"
        );
        // 不带 `/` 的路径也要拼成一个合法 URL，而不是 `base//profile`。
        assert_eq!(
            campaign_portal(CN_BASE, "profile/growth-center").get("referer"),
            Some("https://www.workbuddy.cn/profile/growth-center")
        );
    }

    #[test]
    fn mp_task_and_mp_report_wire_order_is_pinned() {
        // 两张表都是「照参考实现逐字搬」：改顺序 / 改名就等于改线上形态。
        assert_eq!(
            mp_task().wire_snapshot(),
            format!("X-Client-Platform: miniprogram\nUser-Agent: {MP_USER_AGENT}\n")
        );
        assert_eq!(
            mp_report().wire_snapshot(),
            format!(
                "Content-Type: application/json\n\
                 Accept: application/json\n\
                 X-Client-Product: workbuddy-mp\n\
                 X-Client-Version: {}\n\
                 X-Client-Platform: mp-weixin\n\
                 X-Platform: wechatmp\n",
                crate::modules::task_event::MP_IDE_VERSION
            )
        );
        // 上报头里的版本必须与事件信封同源（不许各自写一个 2.4.0）。
        assert_eq!(
            mp_report().get("X-Client-Version"),
            Some(crate::modules::task_event::MP_IDE_VERSION)
        );
        assert_eq!(crate::modules::task_event::MP_IDE_VERSION, "2.4.0");
    }

    #[test]
    fn chat_wire_order_is_pinned_for_both_regions() {
        let cn = chat(Region::Cn, CN_BASE, &fixed_identity()).expect("cn chat table");
        assert_eq!(
            cn.wire_snapshot(),
            "Accept: application/json, text/event-stream\n\
             Accept-Language: zh-CN\n\
             X-CodeBuddy-Request: 1\n\
             X-Agent-Purpose: conversation\n\
             X-Requested-With: XMLHttpRequest\n\
             Origin: https://www.workbuddy.cn\n\
             Referer: https://www.workbuddy.cn/\n\
             Content-Type: application/json\n\
             User-Agent: WorkBuddy/5.5.6 WorkBuddy/5.5.6\n"
        );
        let global = chat(Region::Global, "https://www.workbuddy.ai", &fixed_identity())
            .expect("global chat table");
        assert_eq!(global.get("Accept-Language"), Some("en-US"));
        assert_eq!(
            global.get("User-Agent"),
            Some("WorkBuddy/5.5.6 WorkBuddy AI/5.5.6")
        );
    }

    /// 形态表与**真正在发**的那套头不能各自演化：chat 表里除 UA 外每一条都必须在
    /// `account::build_chat_headers` 里取到同一个值。
    #[test]
    fn chat_table_matches_the_live_chat_headers() {
        use crate::modules::account::build_chat_headers;
        use serde_json::json;

        let account = json!({
            "uid": "u-1",
            "access_token": "tok",
            "domain": "d",
            "enterpriseId": "e-1",
        });
        let live = build_chat_headers(Region::Cn, &account);
        let table = chat(Region::Cn, region_spec(Region::Cn).billing_base, &fixed_identity())
            .expect("chat table");
        for (name, value) in table.iter() {
            if name.eq_ignore_ascii_case("user-agent") {
                continue;
            }
            assert_eq!(
                live.get(name).map(String::as_str),
                Some(value),
                "chat 表里的 {name} 与 build_chat_headers 不一致"
            );
        }
        // 反过来：形态表不许漏掉 live 里的**框架**头（身份头由 account 层负责）。
        for name in [
            "Accept",
            "Accept-Language",
            "X-CodeBuddy-Request",
            "X-Agent-Purpose",
            "X-Requested-With",
            "Origin",
            "Referer",
            "Content-Type",
        ] {
            assert!(
                table.get(name).is_some(),
                "chat 表缺框架头 {name}（live 里有）"
            );
        }
    }

    #[test]
    fn chat_ua_follows_identity_never_a_literal_version() {
        let a = chat(
            Region::Cn,
            CN_BASE,
            &ChatIdentity {
                client_version: "5.5.6".into(),
                cli_version: None,
            },
        )
        .expect("a");
        let b = chat(
            Region::Cn,
            CN_BASE,
            &ChatIdentity {
                client_version: "6.0.1".into(),
                cli_version: None,
            },
        )
        .expect("b");
        assert_ne!(a.get("User-Agent"), b.get("User-Agent"));
        assert_eq!(a.get("User-Agent"), Some("WorkBuddy/5.5.6 WorkBuddy/5.5.6"));
        // 非法版本必须报错，不能拼出 `WorkBuddy/` 这种空版本 UA。
        assert!(chat(
            Region::Cn,
            CN_BASE,
            &ChatIdentity {
                client_version: String::new(),
                cli_version: None,
            }
        )
        .is_err());
        // 生产代码里不许出现写死的 `WorkBuddy/<数字>` 版本字面量（测试段自己当然要写期望值）。
        let source = include_str!("outbound_headers.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        for (index, _) in production.match_indices("WorkBuddy/") {
            let rest = &production[index + "WorkBuddy/".len()..];
            assert!(
                !rest.starts_with(|c: char| c.is_ascii_digit()),
                "outbound_headers.rs 里出现了写死版本的 UA 字面量"
            );
        }
    }

    /// UA 只在 `identity` / `config` / 本模块三处出现：其它出站模块不许再抄浏览器 UA 或
    /// 换一份 CLI/小程序 UA。
    #[test]
    fn no_ua_is_hardcoded_outside_this_module() {
        for (name, source) in [
            ("activity.rs", include_str!("activity.rs")),
            ("cat.rs", include_str!("cat.rs")),
        ] {
            assert!(
                !source.contains("Mozilla/5.0"),
                "{name} 里还有写死的浏览器/小程序 UA，应改用 outbound_headers"
            );
            assert!(
                !source.contains("WorkBuddy/"),
                "{name} 里还有写死的产品 UA token，应改用 outbound_headers"
            );
        }
        let upstream = include_str!("upstream.rs");
        assert!(
            !upstream.contains("\"CLI/"),
            "upstream.rs 里还有写死的 CLI UA，应改用 outbound_headers::CLI_USER_AGENT"
        );
    }

    #[test]
    fn header_sets_carry_no_credentials() {
        let tables = [
            desktop(CN_BASE),
            cli(CN_BASE),
            mp(CN_BASE),
            campaign_portal(CN_BASE, GROWTH_CENTER_PATH),
            chat(Region::Cn, CN_BASE, &fixed_identity()).expect("chat"),
        ];
        for table in &tables {
            let snapshot = table.wire_snapshot().to_ascii_lowercase();
            for needle in ["bearer", "authorization", "token", "cookie", "password", "secret"] {
                assert!(
                    !snapshot.contains(needle),
                    "出站头表里出现了与凭据有关的内容：{needle}"
                );
            }
        }
    }

    #[test]
    fn set_replaces_in_place_and_push_keeps_duplicates() {
        let mut set = HeaderSet::new();
        set.push("origin", "a").push("referer", "b");
        set.set("Origin", "c");
        assert_eq!(set.len(), 2);
        assert_eq!(set.get("ORIGIN"), Some("c"));
        assert_eq!(set.names(), vec!["origin", "referer"]);
        set.push("x-multi", "1").push("x-multi", "2");
        assert_eq!(set.len(), 4);
        assert_eq!(
            set.wire_snapshot(),
            "origin: c\nreferer: b\nx-multi: 1\nx-multi: 2\n"
        );
        // `to_map` 与逐条 insert 同义：同名最后一条生效。
        assert_eq!(set.to_map().get("x-multi").map(String::as_str), Some("2"));
        assert!(HeaderSet::new().is_empty());
    }

    #[test]
    fn surface_table_dispatches_and_requires_a_chat_identity() {
        let base = region_base(Region::Cn);
        assert_eq!(
            Surface::Desktop.table(Region::Cn, base, None).expect("desktop"),
            desktop(base)
        );
        assert_eq!(
            Surface::Mp.table(Region::Cn, base, None).expect("mp"),
            mp(base)
        );
        assert_eq!(
            Surface::CampaignPortal
                .table(Region::Cn, base, None)
                .expect("portal"),
            campaign_portal(base, GROWTH_CENTER_PATH)
        );
        // chat 没有身份 ⇒ 报错（不拿兜底版本顶替）。
        let error = Surface::Chat
            .table(Region::Cn, base, None)
            .expect_err("chat without identity must fail");
        assert!(error.contains("chat identity"));
        assert_eq!(
            Surface::Chat
                .table(Region::Cn, base, Some(&fixed_identity()))
                .expect("chat"),
            chat(Region::Cn, base, &fixed_identity()).expect("chat")
        );
    }
}
