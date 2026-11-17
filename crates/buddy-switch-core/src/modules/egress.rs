//! 账号级**出口槽**（`egress_slot`，opt-in、缺省直连）。
//!
//! 设计依据：`docs/specs/WB-AUTOMATION-ARCH.md` §7.2（参考基线 `wb2api@22b7966` 的
//! `proxy_slots` / `wb_egress`）。四句话说完这一层：
//!
//! 1. **槽是用户自己的命名代理列表**：`slot-N` + `scheme`（http/https/socks5/socks5h）+
//!   `host` + `port` + 加密凭据 + 地区提示 + `enabled`。账号记录里只存**槽 id**
//!   （`automation.egress_slot`），不存 URL —— 改 URL 不改变绑定，删槽必须显式解绑。
//! 2. **槽 id 单调分配、不复用**：删掉 `slot-2` 再新建若复用同一个 id，所有绑 `slot-2`
//!   的账号会被**静默改道**。所以 id 由 `next_index` 发号，只增不减。
//! 3. **凭据加密落盘**：代理用户名/密码用 `at_rest::seal_field`（AES-256-GCM，本机密钥
//!   `~/.buddy-switch/egress_key.json`，0600）封成信封后存进槽表；明文只在建连那一刻存在于
//!   内存里。`Debug` / `Display` / 错误串一律打码，日志用 [`redact_proxy_url`]。
//! 4. **默认直连、关掉立即生效**：未绑定 / 槽被删 / 槽被禁用 / id 非法 ⇒ [`Resolution::direct`]，
//!   与今日行为逐字一致；账号与槽的绑定关系**每轮重读**账号 JSON，不缓存开关。
//!
//! ⚠️ **未覆盖网关中继**：`crates/buddy-switch-gateway` 的 `upstream_via_proxy` 仍是**全局**
//! 开关（中继层按 API key 选账号、只有一份 `reqwest::Client`）。账号级槽先覆盖**本仓自己的
//! 账号请求**（刷新 / 任务链 / 需要账号身份的查询）；网关改造成按 uid 取客户端是独立一步，
//! 见 `WB-AUTOMATION-ARCH.md` §7.2.1 的「尚未落地」。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::modules::account;
use crate::modules::at_rest::{decrypt_field, seal_field};
use crate::modules::backoff::EgressKey;
use crate::modules::config::{
    atomic_write, direct_client, egress_key_file, egress_slots_file, http_client_builder, now_ms,
};
use crate::modules::region::Region;
use crate::modules::runtime_log::{warn, REDACTED};

/// 槽 id 前缀（与 `backoff::EgressKey::slot` 的字符集约定一致）。
pub const SLOT_PREFIX: &str = "slot-";

/// 槽表 schema 版本。
pub const SLOTS_FILE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// 协议
// ---------------------------------------------------------------------------

/// 槽的代理协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressScheme {
    Http,
    Https,
    Socks5,
    /// socks5 + **远端 DNS**（`socks5h`；域名在代理侧解析，避免本地 DNS 泄露地区）。
    Socks5h,
}

impl EgressScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }

    /// 解析协议名（大小写不敏感）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks5" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 8080,
            Self::Https => 8443,
            Self::Socks5 | Self::Socks5h => 1080,
        }
    }
}

// ---------------------------------------------------------------------------
// 出口情报（住宅 / 机房、国家、ISP）
// ---------------------------------------------------------------------------

/// 一次出口探测的结论。**每一个字段都可能为空**，空 = 不知道，绝不猜。
///
/// 来源（参考实现 `wb_ipintel.py`）：出口 IP 由槽自己访问 `api.ipify.org` 得到，
/// 国家 / 类型 / ISP 由 `ip-api.com`（`lang=zh-CN`）直连查得。查询必须**直连**：
/// 经槽走会被它自己的出口回答，也查不出「这条槽到底从哪出去」。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotIntel {
    pub exit_ip: String,
    /// 国家名（中文，展示用）。
    pub country: String,
    /// ISO 国家码（大写，按国家分组用）。
    pub country_code: String,
    /// `residential` / `datacenter`；空 = `hosting` 字段没给，不知道。
    pub ip_type: String,
    pub isp: String,
    pub asn: String,
    /// 探测时刻；`0` = 从未探测。
    pub probed_at: i64,
}

/// 空情报（查询没成 / 还没有出口 IP）。形状与成功的一致，调用方不需要分支。
pub fn empty_intel() -> SlotIntel {
    SlotIntel::default()
}

/// ip-api.com 的回复 → 槽要存的字段。任何不可用的形状一律返回空，**不猜**。
///
/// `hosting` 只是启发式（机房托管商宣告的网段），不是注册表事实；所以同时存 ASN/ISP
/// 让操作者自己看。
pub fn classify_intel(payload: &Value, exit_ip: &str, probed_at: i64) -> SlotIntel {
    let mut intel = empty_intel();
    intel.exit_ip = exit_ip.trim().to_string();
    intel.probed_at = probed_at;
    if payload.get("status").and_then(Value::as_str) != Some("success") {
        return intel;
    }
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    intel.country = text("country");
    intel.country_code = text("countryCode").to_ascii_uppercase();
    intel.ip_type = match payload.get("hosting").and_then(Value::as_bool) {
        Some(true) => "datacenter".to_string(),
        Some(false) => "residential".to_string(),
        None => String::new(),
    };
    intel.isp = {
        let isp = text("isp");
        if isp.is_empty() {
            text("org")
        } else {
            isp
        }
    };
    intel.asn = text("as");
    intel
}

/// 地址类型的展示名（`住宅` / `机房`）；未知 ⇒ 空串（不要给它编一个名字）。
pub fn ip_type_label(ip_type: &str) -> &'static str {
    match ip_type.trim().to_ascii_lowercase().as_str() {
        "residential" => "住宅",
        "datacenter" => "机房",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// 槽模型
// ---------------------------------------------------------------------------

/// 一个出口槽。**凭据只以密文形式存在**（[`EgressSlot::credential`] 是 at-rest 信封）。
#[derive(Clone)]
pub struct EgressSlot {
    id: String,
    label: String,
    scheme: EgressScheme,
    host: String,
    port: u16,
    /// `at_rest::seal_field` 产出的信封（`用户名:密码`）；`None` = 无认证。
    credential: Option<String>,
    /// 探测得到的地区提示（只用于展示与后续 geo；**不参与路由判定**）。
    region: Option<String>,
    enabled: bool,
    /// 最近一次探测的出口情报（**不参与路由判定**，只用于展示与分组）。
    intel: SlotIntel,
}

impl EgressSlot {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn scheme(&self) -> EgressScheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 最近一次探测的出口情报（空字段 = 不知道）。
    pub fn intel(&self) -> &SlotIntel {
        &self.intel
    }

    /// 是否有凭据（**只回答有没有**，不返回内容）。
    pub fn has_credential(&self) -> bool {
        self.credential.is_some()
    }

    /// 不带凭据的地址（**可安全打日志**）：`scheme://host:port`。
    pub fn base_url(&self) -> String {
        format!(
            "{}://{}:{}",
            self.scheme.as_str(),
            host_for_url(&self.host),
            self.port
        )
    }

    /// 真正用来建连的代理地址（含凭据）。
    ///
    /// ⚠️ **返回值不许进日志/错误串**：调用方要打印请用 [`redact_proxy_url`]。
    /// 用 `reqwest::Url` 组装而不是字符串拼接，是为了让用户名/密码里的 `@`、`/`、`:` 与
    /// IPv6 字面量都被正确编码（手拼在这里踩过坑）。
    pub fn proxy_url(&self, credential: Option<(&str, &str)>) -> Result<String, String> {
        let raw = self.base_url();
        let mut url = reqwest::Url::parse(&raw)
            .map_err(|_| format!("egress {}: 地址无效", self.id))?;
        if let Some((user, pass)) = credential {
            url.set_username(user)
                .map_err(|_| format!("egress {}: 用户名无效", self.id))?;
            url.set_password(Some(pass))
                .map_err(|_| format!("egress {}: 密码无效", self.id))?;
        }
        Ok(url.to_string())
    }

    /// 落盘形态（凭据仍是密文）。
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "scheme": self.scheme.as_str(),
            "host": self.host,
            "port": self.port,
            "credential": self.credential,
            "region": self.region,
            "enabled": self.enabled,
            "exit_ip": self.intel.exit_ip,
            "country": self.intel.country,
            "country_code": self.intel.country_code,
            "ip_type": self.intel.ip_type,
            "isp": self.intel.isp,
            "asn": self.intel.asn,
            "probed_at": self.intel.probed_at,
        })
    }

    /// 从 JSON 解析；**任何一个字段不合理就整条拒绝**（宁可少一个槽，不要半残的槽）。
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let id = text_field(value, "id")?;
        // 复用熔断键的校验：id 字符集与「不能与 direct 撞名」是同一套规则。
        EgressKey::slot(&id)?;
        if !id.starts_with(SLOT_PREFIX) {
            return Err(format!("egress slot id must start with '{SLOT_PREFIX}'"));
        }
        let scheme_raw = text_field(value, "scheme")?;
        let scheme = EgressScheme::parse(&scheme_raw)
            .ok_or_else(|| format!("egress {id}: unsupported scheme"))?;
        let host = text_field(value, "host")?;
        validate_host(&host).map_err(|reason| format!("egress {id}: {reason}"))?;
        let port = value
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("egress {id}: port must be 1..=65535"))?;
        let label = value
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{host}:{port}"));
        let credential = value
            .get("credential")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|envelope| !envelope.is_empty())
            .map(str::to_string);
        let region = value
            .get("region")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|region| !region.is_empty())
            .map(str::to_string);
        let enabled = value.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        // 情报是**纯展示**：缺字段 / 类型不对都只是「不知道」，不能让整条槽解析失败。
        let intel_text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string()
        };
        let intel = SlotIntel {
            exit_ip: intel_text("exit_ip"),
            country: intel_text("country"),
            country_code: intel_text("country_code"),
            ip_type: intel_text("ip_type"),
            isp: intel_text("isp"),
            asn: intel_text("asn"),
            probed_at: value.get("probed_at").and_then(Value::as_i64).unwrap_or(0),
        };
        Ok(Self {
            id,
            label,
            scheme,
            host,
            port,
            credential,
            region,
            enabled,
            intel,
        })
    }
}

impl fmt::Debug for EgressSlot {
    /// 打码：凭据信封不进日志（它是密文，但泄露密文 + 本机密钥文件就等于泄露凭据）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EgressSlot")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("credential", &self.credential.as_ref().map(|_| REDACTED))
            .field("region", &self.region)
            .field("enabled", &self.enabled)
            .field("intel", &self.intel)
            .finish()
    }
}

impl fmt::Display for EgressSlot {
    /// 展示用：`slot-1 (socks5 10.0.0.2:1080, enabled)` —— **绝不含凭据**。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({} {}:{}, {})",
            self.id,
            self.scheme.as_str(),
            host_for_url(&self.host),
            self.port,
            if self.enabled { "enabled" } else { "disabled" }
        )
    }
}

fn text_field(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("egress slot: missing '{key}'"))
}

/// 主机名/字面量校验：不允许空白、`@`（凭据必须走 `credential` 字段）、`/`、`?`、`#`。
fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("host must not be empty".into());
    }
    if host
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '@' | '/' | '?' | '#'))
    {
        return Err("host must not contain whitespace, '@', '/', '?' or '#'".into());
    }
    Ok(())
}

/// IPv6 字面量在 URL 里必须带方括号。
fn host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

// ---------------------------------------------------------------------------
// 建槽（凭据加密）
// ---------------------------------------------------------------------------

/// 新建槽的输入（凭据是**明文**，只在内存里经过本结构，落盘前已加密）。
pub struct SlotDraft {
    pub label: String,
    pub scheme: EgressScheme,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub region: Option<String>,
}

impl SlotDraft {
    pub fn new(label: impl Into<String>, scheme: EgressScheme, host: impl Into<String>, port: u16) -> Self {
        Self {
            label: label.into(),
            scheme,
            host: host.into(),
            port,
            username: None,
            password: None,
            region: None,
        }
    }

    /// 附加认证（两者都给才算有凭据；只给一个视为调用方 bug，直接报错）。
    pub fn with_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    fn validate(&self) -> Result<(), String> {
        validate_host(self.host.trim())?;
        if self.port == 0 {
            return Err("port must be 1..=65535".into());
        }
        match (&self.username, &self.password) {
            (Some(_), Some(_)) | (None, None) => Ok(()),
            _ => Err("egress slot: username and password must be provided together".into()),
        }
    }
}

/// 本机封凭据用的密钥（32B，`~/.buddy-switch/egress_key.json`，0600）。
///
/// 为什么**不**复用设备身份的盐：两者生命周期不同。用户清理设备身份（换指纹）不应该让
/// 代理凭据变成解不开的密文，反之亦然。
pub fn install_key() -> Result<[u8; 32], String> {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    if let Some(key) = KEY.get() {
        return Ok(*key);
    }
    let key = load_or_create_key(&egress_key_file())?;
    let _ = KEY.set(key);
    Ok(key)
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32], String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|_| "egress key: invalid json".to_string())?;
            let hex = value
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| "egress key: missing field".to_string())?;
            let bytes = hex_decode(hex).ok_or_else(|| "egress key: invalid hex".to_string())?;
            if bytes.len() != 32 {
                return Err("egress key: wrong length".to_string());
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            Ok(key)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; 32];
            key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            let payload = json!({
                "version": 1,
                "key": hex_encode(&key),
                "created_at": now_ms(),
            });
            let text = serde_json::to_string_pretty(&payload)
                .map_err(|_| "egress key: encode failed".to_string())?;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            atomic_write(path, &text).map_err(|_| "egress key: write failed".to_string())?;
            Ok(key)
        }
        // **不**在损坏时重新生成：那会让既有密文凭据永久解不开（静默丢配置比报错更坏）。
        Err(_) => Err("egress key: unreadable".to_string()),
    }
}

/// 把 `用户名:密码` 封成 at-rest 信封。
pub fn seal_credential(username: &str, password: &str, key: &[u8; 32]) -> Result<String, String> {
    if username.is_empty() {
        return Err("egress credential: username must not be empty".into());
    }
    if username.contains(':') {
        return Err("egress credential: username must not contain ':'".into());
    }
    let plaintext = format!("{username}:{password}");
    seal_field(&plaintext, key).map_err(|_| "egress credential: seal failed".to_string())
}

/// `SlotDraft` 里的明文凭据 → 密文信封；没给凭据 ⇒ `None`（不是空信封）。
fn seal_draft_credential(draft: &SlotDraft, key: &[u8; 32]) -> Result<Option<String>, String> {
    match (&draft.username, &draft.password) {
        (Some(user), Some(pass)) => seal_credential(user, pass, key).map(Some),
        _ => Ok(None),
    }
}

/// 解开信封 → `(用户名, 密码)`。失败**不推测**、不回落空凭据。
pub fn open_credential(envelope: &str, key: &[u8; 32]) -> Result<(String, String), String> {
    let plaintext =
        decrypt_field(envelope, key).map_err(|_| "egress credential: decrypt failed".to_string())?;
    plaintext
        .split_once(':')
        .map(|(user, pass)| (user.to_string(), pass.to_string()))
        .ok_or_else(|| "egress credential: malformed payload".to_string())
}

// ---------------------------------------------------------------------------
// 槽表
// ---------------------------------------------------------------------------

/// 全部槽 + 发号器。`next_index` **只增不减**（槽 id 不复用）。
pub struct EgressTable {
    slots: Vec<EgressSlot>,
    next_index: u32,
}

impl Default for EgressTable {
    fn default() -> Self {
        Self::empty()
    }
}

impl EgressTable {
    pub fn empty() -> Self {
        Self {
            slots: Vec::new(),
            next_index: 1,
        }
    }

    /// 生产路径：`~/.buddy-switch/egress_slots.json`（受 `BUDDY_SWITCH_HOME` 影响）。
    pub fn load() -> Self {
        Self::load_from(&egress_slots_file())
    }

    /// 文件缺失 / 损坏 ⇒ **空表**（= 全部直连，也就是今日行为），不 panic。
    ///
    /// 为什么敢 fail-open 到「无槽」：槽只是「从哪出去」，没有它请求照样发，只是走直连；
    /// 而 fail-closed（拒绝发请求）会让一次配置文件写坏变成整仓不可用。
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::empty();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => Self::from_value(&value),
            Err(_) => {
                static WARNED: OnceLock<()> = OnceLock::new();
                if WARNED.set(()).is_ok() {
                    warn("egress", "egress slots file is not valid json; using direct only");
                }
                Self::empty()
            }
        }
    }

    /// 容错解析：单条槽不合法只丢这一条；`next_index` 取「文件里的值」与「现有最大 slot 号 + 1」
    /// 的较大者 —— 手改文件把 `next_index` 改小也不能让 id 复用。
    pub fn from_value(value: &Value) -> Self {
        let mut slots: Vec<EgressSlot> = Vec::new();
        if let Some(items) = value.get("slots").and_then(Value::as_array) {
            for item in items {
                if let Ok(slot) = EgressSlot::from_value(item) {
                    if slots.iter().all(|existing| existing.id() != slot.id()) {
                        slots.push(slot);
                    }
                }
            }
        }
        let stored = value
            .get("next_index")
            .and_then(Value::as_u64)
            .and_then(|index| u32::try_from(index).ok())
            .unwrap_or(1);
        let highest = slots.iter().filter_map(|slot| slot_index(slot.id())).max();
        let next_index = highest
            .map(|highest| highest.saturating_add(1))
            .unwrap_or(1)
            .max(stored)
            .max(1);
        Self { slots, next_index }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "version": SLOTS_FILE_VERSION,
            "next_index": self.next_index,
            "slots": self.slots.iter().map(EgressSlot::to_value).collect::<Vec<_>>(),
        })
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&egress_slots_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.to_value())
            .map_err(|_| "egress slots: encode failed".to_string())?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        atomic_write(path, &text).map_err(|_| "egress slots: write failed".to_string())?;
        // 槽表变了，缓存的客户端可能指向旧地址/旧凭据：整表清掉最省心（重建成本毫秒级）。
        clear_client_cache();
        Ok(())
    }

    pub fn slots(&self) -> &[EgressSlot] {
        &self.slots
    }

    pub fn get(&self, id: &str) -> Option<&EgressSlot> {
        self.slots.iter().find(|slot| slot.id() == id)
    }

    /// 发一个新槽（凭据用本机密钥加密）。返回 `slot-N`。
    pub fn allocate(&mut self, draft: SlotDraft) -> Result<String, String> {
        let key = install_key()?;
        self.allocate_with_key(draft, &key)
    }

    /// 同 [`EgressTable::allocate`]，密钥由调用方给（测试用，不碰主目录）。
    pub fn allocate_with_key(
        &mut self,
        draft: SlotDraft,
        key: &[u8; 32],
    ) -> Result<String, String> {
        draft.validate()?;
        let credential = seal_draft_credential(&draft, key)?;
        let id = format!("{SLOT_PREFIX}{}", self.next_index);
        self.next_index = self.next_index.saturating_add(1);
        self.slots.push(EgressSlot {
            id: id.clone(),
            label: if draft.label.trim().is_empty() {
                format!("{}:{}", draft.host.trim(), draft.port)
            } else {
                draft.label.trim().to_string()
            },
            scheme: draft.scheme,
            host: draft.host.trim().to_string(),
            port: draft.port,
            credential,
            region: draft.region,
            enabled: true,
            intel: empty_intel(),
        });
        Ok(id)
    }

    /// 改槽定义（**id、启用状态、探测情报、既有凭据在未给新凭据时都保留**）。
    ///
    /// 为什么凭据「不给就保留」：改一个代理地址不该要求用户重新输入密码 —— 拿不到密码
    /// 就只能清空凭据，那等于把一条能用的槽改坏。要给新凭据就同时给用户名 + 密码。
    pub fn update(&mut self, id: &str, draft: SlotDraft) -> Result<(), String> {
        let key = install_key()?;
        self.update_with_key(id, draft, &key)
    }

    /// 同 [`EgressTable::update`]，密钥由调用方给（测试用，不碰主目录）。
    pub fn update_with_key(
        &mut self,
        id: &str,
        draft: SlotDraft,
        key: &[u8; 32],
    ) -> Result<(), String> {
        draft.validate()?;
        let new_credential = seal_draft_credential(&draft, key)?;
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.id() == id)
            .ok_or_else(|| format!("egress slot '{id}' not found"))?;
        slot.label = if draft.label.trim().is_empty() {
            format!("{}:{}", draft.host.trim(), draft.port)
        } else {
            draft.label.trim().to_string()
        };
        slot.scheme = draft.scheme;
        slot.host = draft.host.trim().to_string();
        slot.port = draft.port;
        slot.region = draft.region;
        if new_credential.is_some() {
            slot.credential = new_credential;
        }
        Ok(())
    }

    /// 写入一次探测结果。
    ///
    /// **查询失败不抹掉旧情报**：`ip-api.com` 被墙 / 限速是常事，它一抖就清空国家，
    /// 面板会从「美国 住宅」变成「未知」并把操作者的分组依据弄丢。只有「这次拿到了
    /// 出口 IP」时才覆盖出口，且只覆盖**非空**字段（参考实现 `intel_ok` 的同一判据）。
    pub fn set_intel(&mut self, id: &str, intel: &SlotIntel) -> Result<(), String> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.id() == id)
            .ok_or_else(|| format!("egress slot '{id}' not found"))?;
        let fresh_geo = !intel.country.is_empty()
            || !intel.country_code.is_empty()
            || !intel.isp.is_empty()
            || !intel.asn.is_empty()
            || !intel.ip_type.is_empty();
        if !intel.exit_ip.is_empty() {
            slot.intel.exit_ip = intel.exit_ip.clone();
        }
        if fresh_geo {
            slot.intel.country = intel.country.clone();
            slot.intel.country_code = intel.country_code.clone();
            slot.intel.ip_type = intel.ip_type.clone();
            slot.intel.isp = intel.isp.clone();
            slot.intel.asn = intel.asn.clone();
        }
        if intel.probed_at > 0 {
            slot.intel.probed_at = intel.probed_at;
        }
        Ok(())
    }

    /// 启用/禁用（**保留槽定义**：用户数据不静默删除）。
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.id() == id)
            .ok_or_else(|| format!("egress slot '{id}' not found"))?;
        slot.enabled = enabled;
        Ok(())
    }

    /// 删槽。id **不再回收**：`next_index` 已经越过它。
    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let before = self.slots.len();
        self.slots.retain(|slot| slot.id() != id);
        if self.slots.len() == before {
            return Err(format!("egress slot '{id}' not found"));
        }
        Ok(())
    }
}

/// `slot-12` → `Some(12)`；不合格式 → `None`。
fn slot_index(id: &str) -> Option<u32> {
    id.strip_prefix(SLOT_PREFIX)?.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// 账号 → 槽（默认直连）
// ---------------------------------------------------------------------------

/// 一次请求的出口解析结果：熔断键 + （可选）槽定义。
pub struct Resolution {
    key: EgressKey,
    slot: Option<EgressSlot>,
}

impl Resolution {
    /// 直连（= 共享出站客户端，也就是今日行为）。
    pub fn direct() -> Self {
        Self {
            key: EgressKey::direct(),
            slot: None,
        }
    }

    /// 熔断键（`direct` / `slot-N`），可直接喂给 `backoff::EgressBreaker`。
    pub fn key(&self) -> &EgressKey {
        &self.key
    }

    pub fn slot(&self) -> Option<&EgressSlot> {
        self.slot.as_ref()
    }

    /// 稳定标识（日志 / 台账用，不含 URL 与凭据）。
    pub fn as_key(&self) -> &str {
        self.key.as_key()
    }
}

/// 账号绑定的槽 id（`automation.egress_slot`）；缺省 / 空 / 非字符串 ⇒ `None`。
pub fn bound_slot_id(account: &Value) -> Option<&str> {
    account
        .get("automation")
        .and_then(|automation| automation.get("egress_slot"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// 解析账号的出口：**未绑定 / 槽不存在 / 槽禁用 / id 非法 ⇒ 直连**。
///
/// 这四条都归到「直连」而不是报错，是因为「出口」只是可选增强：任何一条不满足就该回到
/// 官方口径（共享出站客户端），而不是让整轮任务发不出去。
pub fn resolve(account: &Value, table: &EgressTable) -> Resolution {
    let Some(id) = bound_slot_id(account) else {
        return Resolution::direct();
    };
    let Some(slot) = table.get(id) else {
        return Resolution::direct();
    };
    if !slot.enabled() {
        return Resolution::direct();
    }
    match EgressKey::slot(slot.id()) {
        Ok(key) => Resolution {
            key,
            slot: Some(slot.clone()),
        },
        Err(_) => Resolution::direct(),
    }
}

/// 生产路径：读槽表 → 解析。槽表很小，按请求读一次磁盘（请求本身有 ≥1s 间隔）。
pub fn resolve_account(account: &Value) -> Resolution {
    resolve(account, &EgressTable::load())
}

/// 按槽 id 解析（**不看 enabled**：探测 / 预览一条停用的槽是合法操作）。
pub fn resolve_slot(id: &str) -> Result<Resolution, String> {
    let table = EgressTable::load();
    let slot = table
        .get(id)
        .ok_or_else(|| format!("egress slot '{id}' not found"))?
        .clone();
    Ok(Resolution {
        key: EgressKey::slot(&slot.id)?,
        slot: Some(slot),
    })
}

/// 绑在某个槽上的账号 id（**扫两个 region**；用于「删槽前先看谁在用」）。
///
/// 只返回 id，不返回账号内容：调用点是确认弹窗，多带一个 token 就多一处泄露面。
pub fn bound_accounts(slot_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for region in [Region::Cn, Region::Global] {
        for account in account::load_accounts_for(region) {
            if bound_slot_id(&account) != Some(slot_id) {
                continue;
            }
            let id = account
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| account.get("uid").and_then(Value::as_str))
                .unwrap_or("")
                .trim();
            if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// 出口探测（ipify + ip-api）
// ---------------------------------------------------------------------------

/// 取「这条槽的出口 IP」的地址（**必须经槽**访问）。
pub const EXIT_IP_PROBE_URL: &str = "https://api.ipify.org";
/// 国家 / 类型 / ISP 的查询入口（**必须直连**，不能经被探测的槽）。
pub const GEO_LOOKUP_URL: &str = "http://ip-api.com/json";
/// 探测超时：ipify 12s、地理查询 8s（参考实现 `probe_proxy_exit` / `LOOKUP_TIMEOUT`）。
pub const EXIT_IP_TIMEOUT_SECS: u64 = 12;
pub const GEO_LOOKUP_TIMEOUT_SECS: u64 = 8;

/// 探测一条槽：先经它取出口 IP，再**直连**查这个 IP 的国家与类型。
///
/// 两步两种结果：`Err` 只表示「经这条槽发不出请求」（槽坏了）；地理查询失败返回
/// [`empty_intel`] 形状（只有 `exit_ip` / `probed_at`），因为一条能跑流量的槽不该被
/// 一个第三方查询的抖动判死。
pub async fn probe_slot(id: &str) -> Result<SlotIntel, String> {
    let resolution = resolve_slot(id)?;
    probe_resolution(&resolution).await
}

/// 同 [`probe_slot`]，但直接对着一次解析结果探测（网关 / 命令层复用）。
pub async fn probe_resolution(resolution: &Resolution) -> Result<SlotIntel, String> {
    let label = resolution.as_key().to_string();
    let client = client_for(resolution).map_err(|error| format!("出口 {label}: {error}"))?;
    let response = client
        .get(EXIT_IP_PROBE_URL)
        .timeout(std::time::Duration::from_secs(EXIT_IP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|error| {
            // 错误串可能带代理 URL（含凭据）：只留分类，不留原文。
            let cause = if error.is_timeout() {
                "超时"
            } else if error.is_connect() {
                "连接失败"
            } else {
                "请求失败"
            };
            format!("出口 {label}: 取不到出口 IP（{cause}）")
        })?;
    if !response.status().is_success() {
        return Err(format!(
            "出口 {label}: 取不到出口 IP（HTTP {}）",
            response.status().as_u16()
        ));
    }
    let exit_ip = response.text().await.unwrap_or_default().trim().to_string();
    if exit_ip.is_empty() {
        return Err(format!("出口 {label}: 取不到出口 IP（空响应）"));
    }
    let mut intel = lookup_intel(&exit_ip).await;
    intel.exit_ip = exit_ip;
    intel.probed_at = now_ms();
    Ok(intel)
}

/// 直连查询一个 IP 的国家 / 类型 / ISP。**任何失败都返回空字段**，不 panic、不重试。
pub async fn lookup_intel(exit_ip: &str) -> SlotIntel {
    let address = exit_ip.trim();
    if address.is_empty() {
        return empty_intel();
    }
    let url = format!(
        "{GEO_LOOKUP_URL}/{}?lang=zh-CN&fields=status,message,country,countryCode,isp,org,as,hosting",
        urlencode(address)
    );
    let Ok(response) = direct_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(GEO_LOOKUP_TIMEOUT_SECS))
        .send()
        .await
    else {
        return empty_intel();
    };
    let Ok(payload) = response.json::<Value>().await else {
        return empty_intel();
    };
    classify_intel(&payload, address, 0)
}

/// 只保留 URL 安全字符（IP 字面量只会用到字母、数字、`.`、`:`），避免拼出第二个查询串。
fn urlencode(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '-'))
        .collect()
}

/// 探测 + 落盘：命令层一行调用。返回合并后的情报（查询失败时是旧值 + 新出口 IP）。
pub fn apply_intel(id: &str, intel: &SlotIntel) -> Result<SlotIntel, String> {
    let mut table = EgressTable::load();
    table.set_intel(id, intel)?;
    let merged = table
        .get(id)
        .map(|slot| slot.intel().clone())
        .unwrap_or_else(empty_intel);
    table.save()?;
    Ok(merged)
}

/// 账号是否打开了**出口级**瞬时失败退避（`automation.egress_backoff`）。
///
/// 缺省关：关着的时候请求路径与改造前逐字一致（不读槽表、不碰熔断表）。
pub fn backoff_enabled(account: &Value) -> bool {
    account
        .get("automation")
        .and_then(|automation| automation.get("egress_backoff"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 进程级出口熔断器（纯内存，重启即归零）。
///
/// 为什么是**进程级**而不是按账号：熔断的对象是「这条出口」，同一出口上的多个账号共享
/// 一次抖动，正是不该让每个账号各领一次 30 分钟冷却的原因
/// （`docs/specs/WB-AUTOMATION-ARCH.md` §7.2）。
pub static BREAKER: std::sync::LazyLock<crate::modules::backoff::EgressBreaker> =
    std::sync::LazyLock::new(crate::modules::backoff::EgressBreaker::with_defaults);

// ---------------------------------------------------------------------------
// 客户端缓存（按槽）
// ---------------------------------------------------------------------------

static CLIENTS: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();

fn clients() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 清空缓存（槽表变更 / 关闭开关时调用）。
pub fn clear_client_cache() {
    if let Ok(mut map) = clients().lock() {
        map.clear();
    }
}

/// 缓存里的客户端数量（诊断用）。
pub fn cached_client_count() -> usize {
    clients().lock().map(|map| map.len()).unwrap_or(0)
}

/// 缓存键：`槽 id + 代理地址摘要`。
///
/// 带上地址摘要（而不是只用 id），是因为用户改槽 URL 时**绑定不变、出口要变**：
/// 只看 id 会把旧地址的客户端一直用下去。摘要用 SHA-256 前 8 字节，**不含明文凭据**。
pub fn cache_key(slot: &EgressSlot, proxy_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(proxy_url.as_bytes());
    format!("{}#{}", slot.id(), hex_encode(&hasher.finalize()[..8]))
}

/// 按解析结果取客户端：直连 ⇒ 共享客户端；槽 ⇒ 带代理的客户端（按槽缓存）。
///
/// 只有**真的用得上凭据**时才去读本机密钥文件：没有槽 / 槽无认证时，这条路径与今日
/// 完全一致，连一个文件都不会创建。
pub fn client_for(resolution: &Resolution) -> Result<reqwest::Client, String> {
    if resolution
        .slot()
        .map(EgressSlot::has_credential)
        .unwrap_or(false)
    {
        let key = install_key()?;
        client_for_with_key(resolution, &key)
    } else {
        client_for_inner(resolution, None)
    }
}

/// 生产路径：解析账号 → 取客户端。
pub fn client_for_account(account: &Value) -> Result<reqwest::Client, String> {
    client_for(&resolve_account(account))
}

/// 同 [`client_for`]，密钥由调用方给（测试用）。
pub fn client_for_with_key(
    resolution: &Resolution,
    key: &[u8; 32],
) -> Result<reqwest::Client, String> {
    client_for_inner(resolution, Some(key))
}

fn client_for_inner(
    resolution: &Resolution,
    key: Option<&[u8; 32]>,
) -> Result<reqwest::Client, String> {
    let Some(slot) = resolution.slot() else {
        return Ok(direct_client().clone());
    };
    let credential = match slot.credential.as_deref() {
        Some(envelope) => {
            let key = key.ok_or_else(|| format!("egress {}: 本机密钥不可用", slot.id()))?;
            Some(open_credential(envelope, key)?)
        }
        None => None,
    };
    let proxy_url = slot.proxy_url(
        credential
            .as_ref()
            .map(|(user, pass)| (user.as_str(), pass.as_str())),
    )?;
    let cache_key = cache_key(slot, &proxy_url);
    // 缓存键带地址摘要：改 URL 自动换客户端；同一地址重复请求复用同一个 client。
    if let Some(cached) = clients()
        .lock()
        .ok()
        .and_then(|map| map.get(&cache_key).cloned())
    {
        return Ok(cached);
    }
    let proxy = reqwest::Proxy::all(&proxy_url)
        .map_err(|_| format!("egress {}: 代理地址无效", slot.id()))?;
    // `.no_proxy()` 在前，`.proxy()` 在后：显式槽**就是**唯一出口，不叠系统代理。
    let client = http_client_builder()
        .no_proxy()
        .proxy(proxy)
        .build()
        .map_err(|_| format!("egress {}: 客户端创建失败", slot.id()))?;
    if let Ok(mut map) = clients().lock() {
        map.insert(cache_key, client.clone());
    }
    Ok(client)
}

// ---------------------------------------------------------------------------
// 日志脱敏
// ---------------------------------------------------------------------------

/// 代理 URL 打码：`socks5://user:pass@host:1080` → `socks5://<redacted>@host:1080`。
///
/// 只保留「协议 + 主机」，因为排查问题时真正有用的是「走的哪个出口」，不是凭据。
pub fn redact_proxy_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return if trimmed.contains('@') {
            REDACTED.to_string()
        } else {
            trimmed.to_string()
        };
    };
    match rest.rsplit_once('@') {
        Some((_userinfo, host_part)) => format!("{scheme}://{REDACTED}@{host_part}"),
        None => trimmed.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 小工具（与 `device_identity` 同形，但刻意不跨模块共用：hex 是本模块的实现细节）
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks(2) {
        let high = (chunk[0] as char).to_digit(16)?;
        let low = (chunk[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key() -> [u8; 32] {
        [0x42u8; 32]
    }

    fn bound(uid: &str, slot: &str) -> Value {
        json!({"uid": uid, "automation": {"egress_slot": slot}})
    }

    fn table_with(draft: SlotDraft) -> (EgressTable, String) {
        let mut table = EgressTable::empty();
        let id = table.allocate_with_key(draft, &key()).expect("allocate");
        (table, id)
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("wb-egress-{tag}-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scheme_parse_and_default_ports() {
        assert_eq!(EgressScheme::parse("SOCKS5"), Some(EgressScheme::Socks5));
        assert_eq!(EgressScheme::parse(" socks5h "), Some(EgressScheme::Socks5h));
        assert_eq!(EgressScheme::parse("http"), Some(EgressScheme::Http));
        assert_eq!(EgressScheme::parse("ftp"), None);
        assert_eq!(EgressScheme::Socks5.default_port(), 1080);
        assert_eq!(EgressScheme::Http.default_port(), 8080);
    }

    #[test]
    fn slot_rejects_credentials_in_host_and_bad_ports() {
        // 凭据必须走 `credential` 字段：把 `user:pass@host` 塞进 host 是最常见的误配。
        let embedded = json!({
            "id": "slot-1", "scheme": "socks5", "host": "user:pass@10.0.0.2", "port": 1080
        });
        assert!(EgressSlot::from_value(&embedded).is_err());

        for port in [0u64, 70000] {
            let bad = json!({"id": "slot-1", "scheme": "socks5", "host": "10.0.0.2", "port": port});
            assert!(EgressSlot::from_value(&bad).is_err(), "port {port}");
        }

        let bad_scheme =
            json!({"id": "slot-1", "scheme": "ftp", "host": "10.0.0.2", "port": 21});
        assert!(EgressSlot::from_value(&bad_scheme).is_err());

        // id 必须带前缀，且不能与 `direct` 撞名。
        for id in ["1", "direct"] {
            let value =
                json!({"id": id, "scheme": "socks5", "host": "10.0.0.2", "port": 1080});
            assert!(EgressSlot::from_value(&value).is_err(), "id {id}");
        }

        let good = json!({
            "id": "slot-1",
            "scheme": "socks5",
            "host": "10.0.0.2",
            "port": 1080,
            "enabled": false
        });
        let slot = EgressSlot::from_value(&good).expect("valid slot");
        assert_eq!(slot.label(), "10.0.0.2:1080", "缺 label 时回落 host:port");
        assert!(!slot.enabled());
        assert!(!slot.has_credential());
    }

    #[test]
    fn ids_are_monotonic_and_never_reused() {
        let mut table = EgressTable::empty();
        let first = table
            .allocate_with_key(
                SlotDraft::new("home", EgressScheme::Socks5, "10.0.0.2", 1080),
                &key(),
            )
            .expect("first");
        let second = table
            .allocate_with_key(
                SlotDraft::new("office", EgressScheme::Http, "10.0.0.3", 8080),
                &key(),
            )
            .expect("second");
        assert_eq!(first, "slot-1");
        assert_eq!(second, "slot-2");

        table.remove(&second).expect("remove");
        let third = table
            .allocate_with_key(
                SlotDraft::new("again", EgressScheme::Socks5, "10.0.0.4", 1080),
                &key(),
            )
            .expect("third");
        assert_eq!(third, "slot-3", "删掉 slot-2 后不能复用它");
        assert!(table.set_enabled(&first, false).is_ok());
        assert_eq!(table.get(&first).map(EgressSlot::enabled), Some(false));
        assert!(table.remove("slot-404").is_err());
    }

    #[test]
    fn credential_is_sealed_never_plaintext_and_round_trips() {
        let draft = SlotDraft::new("home", EgressScheme::Socks5, "10.0.0.2", 1080)
            .with_auth("alice", "s3cr3t-pass");
        let (table, id) = table_with(draft);
        let slot = table.get(&id).expect("slot");
        assert!(slot.has_credential());

        // 落盘形态（哪怕整份 JSON 被贴进工单）里不得出现明文。
        let persisted = table.to_value().to_string();
        assert!(!persisted.contains("alice"), "{persisted}");
        assert!(!persisted.contains("s3cr3t-pass"), "{persisted}");
        let rendered = format!("{slot:?} {slot}");
        assert!(!rendered.contains("alice"), "{rendered}");
        assert!(!rendered.contains("s3cr3t-pass"), "{rendered}");
        assert!(rendered.contains(REDACTED));

        // 密文可解密回原文（只有拿本机密钥才解得开）。
        let envelope = slot
            .credential
            .as_deref()
            .expect("credential envelope");
        assert_eq!(
            open_credential(envelope, &key()).expect("open"),
            ("alice".to_string(), "s3cr3t-pass".to_string())
        );
        assert!(
            open_credential(envelope, &[0x43u8; 32]).is_err(),
            "换一把密钥必须解不开"
        );
    }

    #[test]
    fn resolve_defaults_to_direct_and_fails_closed() {
        let (table, id) = table_with(SlotDraft::new(
            "home",
            EgressScheme::Socks5,
            "10.0.0.2",
            1080,
        ));

        // 未绑定 ⇒ 直连。
        let unbound = json!({"uid": "u1"});
        assert_eq!(resolve(&unbound, &table).as_key(), "direct");
        // 绑定存在且启用 ⇒ slot-N。
        assert_eq!(resolve(&bound("u1", &id), &table).as_key(), id.as_str());
        // 绑定到不存在的槽 ⇒ 直连（不报错、不改道）。
        assert_eq!(resolve(&bound("u1", "slot-404"), &table).as_key(), "direct");
        // 槽被禁用 ⇒ 直连（关掉立即生效）。
        let mut disabled = EgressTable::empty();
        disabled
            .allocate_with_key(
                SlotDraft::new("home", EgressScheme::Socks5, "10.0.0.2", 1080),
                &key(),
            )
            .expect("allocate");
        disabled.set_enabled(&id, false).expect("disable");
        assert_eq!(resolve(&bound("u1", &id), &disabled).as_key(), "direct");
        // id 非法（字符集）⇒ 直连。
        assert_eq!(resolve(&bound("u1", "slot 1"), &table).as_key(), "direct");
    }

    #[test]
    fn binding_one_account_does_not_affect_the_others() {
        let (table, id) = table_with(SlotDraft::new(
            "home",
            EgressScheme::Socks5,
            "10.0.0.2",
            1080,
        ));
        assert_eq!(resolve(&bound("u1", &id), &table).as_key(), id.as_str());
        // 同表里的另一个账号：没绑定就是直连，不会「继承」别人的槽。
        assert_eq!(resolve(&json!({"uid": "u2"}), &table).as_key(), "direct");
    }

    #[test]
    fn clients_are_cached_per_slot_and_rebuilt_when_the_url_changes() {
        clear_client_cache();
        let mut table = EgressTable::empty();
        let id = table
            .allocate_with_key(
                SlotDraft::new("home", EgressScheme::Socks5, "10.0.0.2", 1080),
                &key(),
            )
            .expect("allocate");
        let resolution = resolve(&bound("u1", &id), &table);
        // 同一个解析结果连续取两次都要成功（第二次命中缓存）。
        let first = client_for_with_key(&resolution, &key()).expect("client");
        let again = client_for_with_key(&resolution, &key()).expect("client again");
        assert!(cached_client_count() >= 1, "槽客户端应进缓存");
        drop((first, again));

        // 改端口 = 改出口：缓存键必须变（否则会一直用旧地址）。
        let slot = table.get(&id).expect("slot");
        let url_a = slot.proxy_url(None).expect("url");
        let moved = EgressSlot::from_value(&json!({
            "id": id,
            "scheme": "socks5",
            "host": "10.0.0.2",
            "port": 1081
        }))
        .expect("slot");
        let url_b = moved.proxy_url(None).expect("url");
        assert_ne!(cache_key(slot, &url_a), cache_key(&moved, &url_b));
        assert_eq!(cache_key(&moved, &url_b), cache_key(&moved, &url_b));

        // 直连解析拿的是共享客户端，不进槽缓存。
        let direct = client_for_with_key(&Resolution::direct(), &key()).expect("direct");
        drop(direct);
    }

    #[test]
    fn redaction_hides_proxy_credentials() {
        let redacted = redact_proxy_url("socks5://alice:s3cr3t@10.0.0.2:1080");
        assert!(!redacted.contains("alice"), "{redacted}");
        assert!(!redacted.contains("s3cr3t"), "{redacted}");
        assert_eq!(redacted, format!("socks5://{REDACTED}@10.0.0.2:1080"));

        // 密码里带 `@` / `:` 也不能漏。
        let redacted = redact_proxy_url("http://u:p@ss@proxy.local:3128");
        assert!(!redacted.contains("p@ss"), "{redacted}");
        // 没有凭据就原样返回（便于排查「走的哪个出口」）。
        assert_eq!(
            redact_proxy_url("http://proxy.local:3128"),
            "http://proxy.local:3128"
        );
        // 没有协议头但含 `@`：宁可整条打码也不猜。
        assert_eq!(redact_proxy_url("alice:pw@host:1"), REDACTED);
    }

    #[test]
    fn ipv6_hosts_are_bracketed_and_urls_never_leak_credentials() {
        let slot = EgressSlot::from_value(&json!({
            "id": "slot-1", "scheme": "socks5h", "host": "::1", "port": 1080
        }))
        .expect("slot");
        assert_eq!(slot.base_url(), "socks5h://[::1]:1080");
        let plain = slot.proxy_url(None).expect("url");
        assert_eq!(plain, "socks5h://[::1]:1080");
        assert!(!plain.contains('@'));
        // 带凭据的地址里 `@` 只有一个（来自 userinfo），密码中的 `@` 会被编码。
        let with_auth = slot.proxy_url(Some(("u", "p@ss"))).expect("url");
        assert_eq!(with_auth.matches('@').count(), 1, "{with_auth}");
        assert!(!with_auth.contains("p@ss"), "{with_auth}");
    }

    #[test]
    fn table_round_trips_and_never_lowers_the_index() {
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("egress_slots.json");
        let mut table = EgressTable::empty();
        table
            .allocate_with_key(
                SlotDraft::new("home", EgressScheme::Socks5, "10.0.0.2", 1080)
                    .with_auth("alice", "pw")
                    .with_region("CN"),
                &key(),
            )
            .expect("allocate");
        table.save_to(&path).expect("save");

        let reloaded = EgressTable::load_from(&path);
        assert_eq!(reloaded.slots().len(), 1);
        let slot = &reloaded.slots()[0];
        assert_eq!(slot.id(), "slot-1");
        assert_eq!(slot.region(), Some("CN"));
        assert!(slot.has_credential());
        // 新槽必须拿 slot-2，而不是复用 slot-1。
        let mut reloaded = reloaded;
        let next = reloaded
            .allocate_with_key(
                SlotDraft::new("office", EgressScheme::Http, "10.0.0.3", 8080),
                &key(),
            )
            .expect("allocate");
        assert_eq!(next, "slot-2");

        // 手改文件把 next_index 改小：仍然不能被绕过。
        let broken = json!({
            "version": 1,
            "next_index": 1,
            "slots": [{"id": "slot-7", "scheme": "http", "host": "h", "port": 1}]
        });
        let table = EgressTable::from_value(&broken);
        let mut table = table;
        let id = table
            .allocate_with_key(
                SlotDraft::new("x", EgressScheme::Http, "h2", 2),
                &key(),
            )
            .expect("allocate");
        assert_eq!(id, "slot-8");

        // 坏 JSON ⇒ 空表（= 全部直连），不 panic。
        std::fs::write(&path, "{not json").expect("write");
        assert!(EgressTable::load_from(&path).slots().is_empty());
    }

    #[test]
    fn update_keeps_the_credential_enabled_flag_and_intel() {
        let (mut table, id) = table_with(
            SlotDraft::new("home", EgressScheme::Socks5, "10.0.0.2", 1080).with_auth("alice", "pw"),
        );
        let envelope_before = table.get(&id).expect("slot").to_value();
        let credential_before = envelope_before["credential"].clone();
        table
            .set_intel(
                &id,
                &SlotIntel {
                    exit_ip: "1.2.3.4".into(),
                    country: "美国".into(),
                    country_code: "US".into(),
                    ip_type: "residential".into(),
                    isp: "Comcast".into(),
                    asn: "AS7922".into(),
                    probed_at: 1_700_000_000_000,
                },
            )
            .expect("set intel");
        table.set_enabled(&id, false).expect("disable");

        // 只换地址、不给凭据：凭据 / 启用状态 / 情报都必须原样保留。
        table
            .update_with_key(
                &id,
                SlotDraft::new("new label", EgressScheme::Http, "10.0.0.9", 8080),
                &key(),
            )
            .expect("update");
        let slot = table.get(&id).expect("slot");
        assert_eq!(slot.host(), "10.0.0.9");
        assert_eq!(slot.port(), 8080);
        assert_eq!(slot.scheme(), EgressScheme::Http);
        assert_eq!(slot.label(), "new label");
        assert!(!slot.enabled(), "更新定义不许顺手把槽打开");
        assert_eq!(slot.to_value()["credential"], credential_before);
        assert_eq!(slot.intel().country, "美国");
        assert_eq!(slot.intel().country_code, "US");

        // 给了新凭据才换。
        table
            .update_with_key(
                &id,
                SlotDraft::new("home", EgressScheme::Http, "10.0.0.9", 8080)
                    .with_auth("bob", "pw2"),
                &key(),
            )
            .expect("update");
        let envelope = table.get(&id).expect("slot").to_value();
        assert_ne!(envelope["credential"], credential_before);
        assert!(table
            .update_with_key(
                "slot-99",
                SlotDraft::new("x", EgressScheme::Http, "h", 1),
                &key()
            )
            .is_err());
    }

    #[test]
    fn intel_survives_a_failed_lookup_and_round_trips() {
        let (mut table, id) = table_with(SlotDraft::new("home", EgressScheme::Http, "h", 80));
        let good = SlotIntel {
            exit_ip: "1.2.3.4".into(),
            country: "美国".into(),
            country_code: "US".into(),
            ip_type: "datacenter".into(),
            isp: "ISP".into(),
            asn: "AS1".into(),
            probed_at: 5,
        };
        table.set_intel(&id, &good).expect("set intel");

        // 地理查询失败（全空）：只更新出口 IP / 时间，旧国家不能丢。
        let degraded = SlotIntel {
            exit_ip: "5.6.7.8".into(),
            probed_at: 6,
            ..SlotIntel::default()
        };
        table.set_intel(&id, &degraded).expect("set intel");
        let intel = table.get(&id).expect("slot").intel().clone();
        assert_eq!(intel.exit_ip, "5.6.7.8");
        assert_eq!(intel.country, "美国");
        assert_eq!(intel.country_code, "US");
        assert_eq!(intel.probed_at, 6);

        // 落盘再读回：情报是展示字段，缺字段也只是「不知道」。
        let dir = TempDir::new("intel");
        let path = dir.0.join("egress_slots.json");
        table.save_to(&path).expect("save");
        let reloaded = EgressTable::load_from(&path);
        assert_eq!(reloaded.get(&id).expect("slot").intel(), &intel);

        // 老文件没有情报字段时整条槽仍然可用（不能因为「不知道」被丢）。
        let legacy = json!({"version": 1, "next_index": 2,
            "slots": [{"id": "slot-1", "scheme": "http", "host": "h", "port": 80}]});
        let table = EgressTable::from_value(&legacy);
        assert_eq!(table.slots().len(), 1);
        assert_eq!(table.get("slot-1").expect("slot").intel(), &empty_intel());
    }

    #[test]
    fn classify_intel_never_guesses() {
        // 非 success / 缺 hosting ⇒ 该字段空，而不是猜成住宅。
        assert_eq!(
            classify_intel(&json!({"status": "fail"}), "1.2.3.4", 9),
            SlotIntel {
                exit_ip: "1.2.3.4".into(),
                probed_at: 9,
                ..SlotIntel::default()
            }
        );
        let intel = classify_intel(
            &json!({
                "status": "success",
                "country": "美国",
                "countryCode": "us",
                "isp": "",
                "org": "Example Org",
                "as": "AS7922 Comcast",
                "hosting": true
            }),
            " 1.2.3.4 ",
            9,
        );
        assert_eq!(intel.exit_ip, "1.2.3.4");
        assert_eq!(intel.country_code, "US");
        assert_eq!(intel.isp, "Example Org", "isp 缺失时回落 org");
        assert_eq!(intel.ip_type, "datacenter");
        assert_eq!(intel.asn, "AS7922 Comcast");
        assert_eq!(intel.probed_at, 9);

        // hosting 缺席 ⇒ 类型未知（不猜住宅）。
        let unknown = classify_intel(&json!({"status": "success"}), "1.1.1.1", 1);
        assert_eq!(unknown.ip_type, "");

        assert_eq!(ip_type_label("residential"), "住宅");
        assert_eq!(ip_type_label("DATACENTER"), "机房");
        assert_eq!(ip_type_label(""), "");
        assert_eq!(ip_type_label("whatever"), "");

        // IP 拼接只保留安全字符：不许拼出第二个查询参数。
        assert_eq!(urlencode("1.2.3.4?a=b&x"), "1.2.3.4abx");
    }

    #[test]
    fn draft_validation_rejects_half_credentials_and_empty_host() {
        let mut table = EgressTable::empty();
        let mut half = SlotDraft::new("x", EgressScheme::Http, "10.0.0.2", 8080);
        half.username = Some("alice".into());
        assert!(table.allocate_with_key(half, &key()).is_err());

        let empty_host = SlotDraft::new("x", EgressScheme::Http, "   ", 8080);
        assert!(table.allocate_with_key(empty_host, &key()).is_err());

        let zero_port = SlotDraft::new("x", EgressScheme::Http, "10.0.0.2", 0);
        assert!(table.allocate_with_key(zero_port, &key()).is_err());
    }
}
