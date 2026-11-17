//! **System key**（`BUDDY_SWITCH_AUTH_KEY`）在网关侧的校验。
//!
//! 同一个环境变量有两种身份：
//! 1. 管理面登录密钥（`buddy-switch-server::app_auth`）—— 换会话 Cookie；
//! 2. 本模块认识的**系统密钥** —— 直接以 `Authorization: Bearer <key>` 调管理面
//!    与四条网关的 `/v1/*`，不必先在网关里创建一把普通 API Key。
//!
//! 为什么允许它进网关：它是部署方自己注入的密钥，权限本来就覆盖「读走全部凭据」；
//! 再要求它为每个客户端建一把 Key 只会让自动化多一步，并不会增加实际安全边界。
//!
//! # 身份从哪来
//!
//! 系统密钥**没有**普通 Key 的 region / variant / 命名空间字段。各网关因此给它
//! **默认归属**（WB/Qoder = CN，Trae = 默认变体，统一入口 = 全部已启用命名空间）：
//! - 这不是「跨区超级权限」：请求仍需遵守该入口的既有路由规则（统一入口必须带前缀）；
//! - 需要精确归属时，仍应创建普通 API Key。
//!
//! # 比较
//!
//! 与普通 Key 一样用常量时间比较；未设置 / 空白环境变量 ⇒ 系统密钥不存在。
//! 台账里的同值 Key **优先**（调用方先查 Key 库），因此系统密钥不会遮蔽用户 Key。

use std::sync::LazyLock;

use crate::apikey::constant_time_eq;

/// 系统密钥的环境变量名（与管理面登录共用）。
pub const SYSTEM_KEY_ENV: &str = "BUDDY_SWITCH_AUTH_KEY";

/// 系统密钥在用量账本 / 日志里的稳定 id（不写进任何 Key 台账）。
pub const SYSTEM_KEY_ID: &str = "__system__";

/// 进程启动后第一次使用时读取环境变量；未设置 / 空白 ⇒ `None`。
static CONFIGURED: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var(SYSTEM_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
});

fn configured() -> Option<&'static str> {
    CONFIGURED.as_deref()
}

/// 候选值是否是当前进程的系统密钥。
pub fn matches(presented: &str) -> bool {
    matches_with(configured(), presented)
}

/// 纯函数版本：便于测试，不读全局环境。
pub fn matches_with(configured: Option<&str>, presented: &str) -> bool {
    let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let presented = presented.trim();
    if presented.is_empty() {
        return false;
    }
    constant_time_eq(presented.as_bytes(), configured.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_non_empty_key_matches() {
        assert!(matches_with(Some("test-key"), "test-key"));
        assert!(matches_with(Some("  test-key  "), " test-key\n"));
        assert!(!matches_with(Some("test-key"), "test-keY"));
        assert!(!matches_with(Some("test-key"), "test-keyx"));
        assert!(!matches_with(Some("test-key"), ""));
        assert!(!matches_with(None, "test-key"));
        assert!(!matches_with(Some("   "), ""));
        assert!(!matches_with(Some("   "), "   "));
    }

    /// 系统密钥没有台账里的 region/variant/namespace，身份由各入口的默认值补齐。
    /// 这里把「默认值」钉死：改默认区域或把配额/白名单从「全开」改成别的，
    /// 必须同步改本测试，否则就是静默改变系统密钥的权限边界。
    #[test]
    fn system_records_share_one_unlimited_default_identity() {
        use crate::apikey::{ApiKeyRecord, QuotaMode, ScopeMode};
        use crate::qoder::apikey::QoderApiKeyRecord;
        use crate::trae::apikey::TraeApiKeyRecord;
        use crate::unified::apikey::UnifiedApiKeyRecord;
        use buddy_switch_core::modules::qoder::region::QoderRegion;
        use buddy_switch_core::modules::region::Region;
        use buddy_switch_core::modules::trae::variant::TraeVariant;

        let wb = ApiKeyRecord::system(Region::default());
        assert_eq!(wb.id, SYSTEM_KEY_ID);
        assert_eq!(wb.region, Region::Cn, "WorkBuddy 系统密钥默认 CN");
        let wb_policy = wb.policy();
        assert_eq!(wb_policy.quota_mode, QuotaMode::Unlimited);
        assert_eq!(wb_policy.scope_mode, ScopeMode::All);

        let trae = TraeApiKeyRecord::system(TraeVariant::default());
        assert_eq!(trae.id, SYSTEM_KEY_ID);
        assert_eq!(trae.variant, TraeVariant::default());

        let qoder = QoderApiKeyRecord::system(QoderRegion::default());
        assert_eq!(qoder.id, SYSTEM_KEY_ID);
        assert_eq!(qoder.region, QoderRegion::Cn, "Qoder 系统密钥默认 CN");

        let unified = UnifiedApiKeyRecord::system(vec!["wb-cn".to_string()], None);
        assert_eq!(unified.id, SYSTEM_KEY_ID);
        assert_eq!(unified.namespaces, ["wb-cn"]);
        assert!(
            unified.biz_namespace.is_none(),
            "系统密钥不授予 /biz/*（没有模型名前缀可推断命名空间）"
        );
        assert!(unified.is_active());
    }
}
