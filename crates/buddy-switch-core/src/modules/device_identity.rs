//! WorkBuddy 的**按账号派生的设备身份**（opt-in，默认关）。
//!
//! 设计依据：`docs/specs/WB-AUTOMATION-ARCH.md` §7.1（参考基线 `wb2api@22b7966` 的
//! `wb_fingerprint.derive_id`）。三条纪律，读代码前先记住：
//!
//! 1. **默认关、按账号开、关掉立刻生效**：只有账号记录里 `automation.device_identity == true`
//!    时才派生（[`enabled_for`] 每次调用都重读账号 JSON，不缓存开关）。关着的时候一切保持
//!    官方客户端原值 —— 与今日行为逐字一致。
//! 2. **派生值绝不落库、不落日志**：`machineId` / `sessionId` / `requestId` 是
//!    `HKDF-SHA256(安装盐, 账号 uid, realm + 标签 + nonce)` 现算出来的；只有**安装盐**持久化在
//!    `~/.buddy-switch/device_identity_salt.json`（0600 原子写、不进备份/导出/诊断包）。
//!    `Debug` 全部打码 —— 派生值本身就是设备指纹，印进日志等于把它贴到工单里。
//! 3. **只在一个地方挂载**：任务链唯一的出网口 `task_io::send` 调 [`apply`] / [`apply_mp`]；
//!    feature 模块自己不许拼这些头（否则关掉开关时总有一条路还在发旧身份）。
//!
//! 为什么「按 uid 稳定派生」而不是随机：随机会让同一账号在上游看来不断换设备，而同机的多个
//! 账号又会共享一个随机值（多号被关联）。稳定派生两件事一起解决：**同一账号长期固定在同一台
//! 「虚拟设备」上，不同账号天然落在不同机器码上**。
//!
//! 与 `qoder/device.rs` 同形（那边用 MD5 派生专属机器标识），但**不共用盐**：两个产品线的
//! 账号不相干，共用盐会让「换产品」看起来像「同一台设备换了应用」。
//!
//! ⚠️ **头名是临时口径**：`HEADER_*` 常量等在线 spike（`WB-AUTOMATION-ARCH.md` §9 在线、
//! 两枚 CN 账号）与参考实现逐字对齐后定稿。改名字只需改常量，调用方不动；开关默认关，所以
//! 今天没有任何请求真的带上它们。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::modules::config::{atomic_write, device_identity_salt_file, now_ms};
use crate::modules::region::Region;
use crate::modules::runtime_log::{warn, REDACTED};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 安装盐的字节长度（HKDF 的 salt 参数）。
pub const SALT_LEN: usize = 32;
/// 盐文件的 schema 版本。
pub const SALT_FILE_VERSION: u32 = 1;

/// 桌面身份头：机器标识（每账号恒定）。
pub const HEADER_MACHINE_ID: &str = "x-client-machine-id";
/// 桌面身份头：会话标识（每账号恒定，不随进程重启变化）。
pub const HEADER_SESSION_ID: &str = "x-client-session-id";
/// 每请求标识头：每次请求派生新值（同一 nonce 稳定，便于重放比对）。
pub const HEADER_REQUEST_ID: &str = "x-request-id";
/// 小程序身份头：设备标识（uid + realm 派生）。
pub const HEADER_MP_DEVICE_ID: &str = "x-client-mp-device-id";
/// 小程序身份头：会话标识（uid + nick 派生，改昵称才变）。
pub const HEADER_MP_SESSION_ID: &str = "x-client-mp-session-id";

/// HKDF `info` 里的产品域（换产品换身份，避免跨产品关联）。
const HKDF_DOMAIN: &str = "workbuddy";

const LABEL_MACHINE: &str = "machine";
const LABEL_SESSION: &str = "session";
const LABEL_REQUEST: &str = "request";
const LABEL_MP_DEVICE: &str = "mp-device";
const LABEL_MP_SESSION: &str = "mp-session";

// ---------------------------------------------------------------------------
// HKDF-SHA256（RFC 5869，只依赖 sha2，不引入 hkdf/hmac crate）
// ---------------------------------------------------------------------------

/// HMAC-SHA256（RFC 2104）。手写而不是加依赖：仓库已有 `sha2`，而 `hmac` 只为这一个
/// 用途引入新 crate 不划算（约束：不新增依赖）。
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        block_key[..32].copy_from_slice(&digest);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= block_key[index];
        outer_pad[index] ^= block_key[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

/// HKDF-SHA256（extract + expand），`out_len` 上限 `255 * 32`（RFC 5869 §2.3）。
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], out_len: usize) -> Vec<u8> {
    let max_len = 255 * 32;
    let out_len = out_len.min(max_len);
    let prk = hmac_sha256(salt, ikm);
    let mut okm = Vec::with_capacity(out_len);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while okm.len() < out_len {
        let mut input = Vec::with_capacity(previous.len() + info.len() + 1);
        input.extend_from_slice(&previous);
        input.extend_from_slice(info);
        input.push(counter);
        let block = hmac_sha256(&prk, &input);
        okm.extend_from_slice(&block);
        previous = block.to_vec();
        counter = counter.wrapping_add(1);
    }
    okm.truncate(out_len);
    okm
}

// ---------------------------------------------------------------------------
// 派生
// ---------------------------------------------------------------------------

/// 派生 32 字节：`IKM = 账号 uid`，`salt = 安装盐`，`info = 产品域/realm/标签/nonce`。
///
/// 把 realm 放进 `info` 而不是 key：同一个 uid 在两个区是两个账号，派生值必须不同。
pub fn derive_bytes(
    salt: &[u8; SALT_LEN],
    realm: &str,
    uid: &str,
    label: &str,
    nonce: u64,
) -> [u8; 32] {
    let info = format!("{HKDF_DOMAIN}/{realm}/{label}/{nonce}");
    let okm = hkdf_sha256(salt, uid.as_bytes(), info.as_bytes(), 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&okm);
    out
}

/// 派生一个**UUID v4 形状**的标识（36 字符，version=4、variant=RFC 4122）。
///
/// 形状而不是「真随机」是刻意的：官方客户端的这三个值都是 UUID 字符串，形状不符会被
/// 一眼看出；而值域仍由 HKDF 决定（同一 (salt, realm, uid, 标签, nonce) 恒等）。
pub fn derive_uuid(
    salt: &[u8; SALT_LEN],
    realm: &str,
    uid: &str,
    label: &str,
    nonce: u64,
) -> String {
    let bytes = derive_bytes(salt, realm, uid, label, nonce);
    let mut raw = [0u8; 16];
    raw.copy_from_slice(&bytes[..16]);
    raw[6] = (raw[6] & 0x0f) | 0x40; // version 4
    raw[8] = (raw[8] & 0x3f) | 0x80; // variant RFC 4122
    uuid::Uuid::from_bytes(raw).to_string()
}

/// 一个账号的桌面设备身份（`machineId` / `sessionId` / 每请求 `requestId`）。
///
/// **故意不 derive `Debug`**：手写的 [`std::fmt::Debug`] 只打码输出。字段里同时有安装盐与
/// uid，任何「顺手打印一下」都不该让它们出现在日志里。
pub struct DeviceIdentity {
    salt: [u8; SALT_LEN],
    realm: String,
    uid: String,
    machine_id: String,
    session_id: String,
}

impl DeviceIdentity {
    /// 按 (安装盐, realm, uid) 派生。同一三元组恒等 —— 这是「同一账号长期同一台设备」的全部实现。
    pub fn derive(salt: &[u8; SALT_LEN], region: Region, uid: &str) -> Self {
        let realm = region.as_str().to_string();
        let machine_id = derive_uuid(salt, &realm, uid, LABEL_MACHINE, 0);
        let session_id = derive_uuid(salt, &realm, uid, LABEL_SESSION, 0);
        Self {
            salt: *salt,
            realm,
            uid: uid.to_string(),
            machine_id,
            session_id,
        }
    }

    pub fn realm(&self) -> &str {
        &self.realm
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 每请求标识：`nonce` 相同则值相同（测试可复现），不同则值不同（不重复）。
    pub fn request_id(&self, nonce: u64) -> String {
        derive_uuid(&self.salt, &self.realm, &self.uid, LABEL_REQUEST, nonce)
    }

    /// 桌面身份头集合（三个头一律覆盖式写入调用方给的 map）。
    pub fn headers(&self, nonce: u64) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(HEADER_MACHINE_ID.to_string(), self.machine_id.clone());
        headers.insert(HEADER_SESSION_ID.to_string(), self.session_id.clone());
        headers.insert(HEADER_REQUEST_ID.to_string(), self.request_id(nonce));
        headers
    }
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("realm", &self.realm)
            .field("uid", &REDACTED)
            .field("machine_id", &REDACTED)
            .field("session_id", &REDACTED)
            .finish()
    }
}

/// 小程序的稳定信封（`device_id` 按 uid、`session_id` 按 uid + nick）。
///
/// 为什么按 nick 再分一层：小程序侧的会话标识跟随「这个人这个昵称」而不是账号本身，
/// 改昵称的人在小程序里等于换了会话，但**设备**不该跟着变（否则一次改昵称看起来像换机）。
pub struct MpEnvelope {
    realm: String,
    device_id: String,
    session_id: String,
}

impl MpEnvelope {
    pub fn derive(salt: &[u8; SALT_LEN], region: Region, uid: &str, nick: &str) -> Self {
        let realm = region.as_str().to_string();
        let device_id = derive_uuid(salt, &realm, uid, LABEL_MP_DEVICE, 0);
        // nick 折进标签（而不是新机制）：改昵称只换会话标识，设备标识不动。
        let session_label = format!("{LABEL_MP_SESSION}:{}", short_hash(salt, uid, nick));
        let session_id = derive_uuid(salt, &realm, uid, &session_label, 0);
        Self {
            realm,
            device_id,
            session_id,
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(HEADER_MP_DEVICE_ID.to_string(), self.device_id.clone());
        headers.insert(HEADER_MP_SESSION_ID.to_string(), self.session_id.clone());
        headers
    }
}

impl std::fmt::Debug for MpEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpEnvelope")
            .field("realm", &self.realm)
            .field("uid", &REDACTED)
            .field("device_id", &REDACTED)
            .field("session_id", &REDACTED)
            .finish()
    }
}

/// 把任意字符串折成 16 位十六进制摘要（用于把 nick 掺进派生标签，而不是存 nick 明文）。
fn short_hash(salt: &[u8; SALT_LEN], uid: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(uid.as_bytes());
    hasher.update(value.as_bytes());
    hex_encode(&hasher.finalize()[..8])
}

/// 32 字节 → 64 位小写十六进制。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 十六进制（大小写均可）→ 字节；非法字符返回 `None`。
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for chunk in bytes.chunks(2) {
        let high = (chunk[0] as char).to_digit(16)?;
        let low = (chunk[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// 安装盐（at rest）
// ---------------------------------------------------------------------------

/// 安装盐的落地存储。`path` 可注入，便于测试用临时目录（生产走 [`SaltStore::default_store`]）。
pub struct SaltStore {
    path: PathBuf,
}

impl SaltStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 生产路径：`~/.buddy-switch/device_identity_salt.json`（受 `BUDDY_SWITCH_HOME` 影响）。
    pub fn default_store() -> Self {
        Self::at(device_identity_salt_file())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读盐。文件不存在 → `Ok(None)`（首次运行）；**内容损坏 → `Err`**。
    ///
    /// 损坏时**绝不重新生成**：静默换盐等于把该账号变成「新设备」，而且是不可回滚的
    /// 硬失败（官方客户端原值不会自己回来）。错误串只描述类别，不带盐/uid 内容。
    pub fn load(&self) -> Result<Option<[u8; SALT_LEN]>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("device identity salt: unreadable".to_string()),
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| "device identity salt: invalid json".to_string())?;
        let hex = value
            .get("salt")
            .and_then(Value::as_str)
            .ok_or_else(|| "device identity salt: missing field".to_string())?;
        let bytes =
            hex_decode(hex).ok_or_else(|| "device identity salt: invalid hex".to_string())?;
        if bytes.len() != SALT_LEN {
            return Err("device identity salt: wrong length".to_string());
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes);
        Ok(Some(salt))
    }

    /// 读；不存在则随机生成一次并**原子写**（0600）落盘，此后恒等。
    pub fn load_or_create(&self) -> Result<[u8; SALT_LEN], String> {
        if let Some(salt) = self.load()? {
            return Ok(salt);
        }
        // 复用 uuid v4 的随机源（仓库已有 uuid，不额外引入 rand_core 调用面）。
        let mut salt = [0u8; SALT_LEN];
        salt[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        salt[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let payload = json!({
            "version": SALT_FILE_VERSION,
            "salt": hex_encode(&salt),
            "created_at": now_ms(),
        });
        let text = serde_json::to_string_pretty(&payload)
            .map_err(|_| "device identity salt: encode failed".to_string())?;
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        atomic_write(&self.path, &text)
            .map_err(|_| "device identity salt: write failed".to_string())?;
        Ok(salt)
    }
}

static INSTALL_SALT: OnceLock<[u8; SALT_LEN]> = OnceLock::new();

/// 进程内缓存的安装盐（**只缓存成功**；失败下次调用重试，不会把一次写盘抖动钉成永久关闭）。
pub fn install_salt() -> Result<[u8; SALT_LEN], String> {
    if let Some(salt) = INSTALL_SALT.get() {
        return Ok(*salt);
    }
    let salt = SaltStore::default_store().load_or_create()?;
    let _ = INSTALL_SALT.set(salt);
    Ok(salt)
}

// ---------------------------------------------------------------------------
// 挂载点：唯一的「把身份贴到请求上」的地方
// ---------------------------------------------------------------------------

/// 该账号是否开了 `device_identity`（缺省 `false`，即与今日行为一致）。
pub fn enabled_for(account: &Value) -> bool {
    account
        .get("automation")
        .and_then(|automation| automation.get("device_identity"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn account_uid(account: &Value) -> Option<&str> {
    account
        .get("uid")
        .and_then(Value::as_str)
        .filter(|uid| !uid.is_empty())
}

/// 把桌面身份头合进 `headers`（**任务链出网口的唯一调用点**：`task_io::send`）。
///
/// 返回 `true` 表示真的写入了；`false` 表示开关没开 / 账号缺 uid / 盐不可读 —— 三种情况
/// 都是**不发这些头**（fail-closed 到官方原值），不是失败。
pub fn apply(
    account: &Value,
    region: Region,
    nonce: u64,
    headers: &mut HashMap<String, String>,
) -> bool {
    if !enabled_for(account) {
        return false;
    }
    match install_salt() {
        Ok(salt) => apply_with_salt(&salt, account, region, nonce, headers),
        Err(error) => {
            // 只告警一次：盐文件坏了会让每轮都失败，重复刷日志没有新信息。
            static WARNED: OnceLock<()> = OnceLock::new();
            if WARNED.set(()).is_ok() {
                warn("device_identity", error);
            }
            false
        }
    }
}

/// [`apply`] 的纯函数内核（盐由调用方给，测试无需碰文件系统）。
pub fn apply_with_salt(
    salt: &[u8; SALT_LEN],
    account: &Value,
    region: Region,
    nonce: u64,
    headers: &mut HashMap<String, String>,
) -> bool {
    if !enabled_for(account) {
        return false;
    }
    let Some(uid) = account_uid(account) else {
        return false;
    };
    let identity = DeviceIdentity::derive(salt, region, uid);
    headers.insert(HEADER_MACHINE_ID.to_string(), identity.machine_id().to_string());
    headers.insert(HEADER_SESSION_ID.to_string(), identity.session_id().to_string());
    headers.insert(HEADER_REQUEST_ID.to_string(), identity.request_id(nonce));
    true
}

/// 把小程序信封头合进 `headers`（小程序族 feature 的出网口调用）。
pub fn apply_mp(
    account: &Value,
    region: Region,
    nick: &str,
    headers: &mut HashMap<String, String>,
) -> bool {
    if !enabled_for(account) {
        return false;
    }
    match install_salt() {
        Ok(salt) => apply_mp_with_salt(&salt, account, region, nick, headers),
        Err(error) => {
            static WARNED: OnceLock<()> = OnceLock::new();
            if WARNED.set(()).is_ok() {
                warn("device_identity", error);
            }
            false
        }
    }
}

/// [`apply_mp`] 的纯函数内核。
pub fn apply_mp_with_salt(
    salt: &[u8; SALT_LEN],
    account: &Value,
    region: Region,
    nick: &str,
    headers: &mut HashMap<String, String>,
) -> bool {
    if !enabled_for(account) {
        return false;
    }
    let Some(uid) = account_uid(account) else {
        return false;
    };
    let envelope = MpEnvelope::derive(salt, region, uid, nick);
    headers.insert(HEADER_MP_DEVICE_ID.to_string(), envelope.device_id().to_string());
    headers.insert(HEADER_MP_SESSION_ID.to_string(), envelope.session_id().to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt(byte: u8) -> [u8; SALT_LEN] {
        [byte; SALT_LEN]
    }

    fn opt_in(uid: &str) -> Value {
        json!({"uid": uid, "automation": {"device_identity": true}})
    }

    /// RFC 5869 Test Case 1：证明这是**真的 HKDF**，不是「sha256 拼一拼」。
    #[test]
    fn hkdf_matches_rfc5869_test_case_1() {
        let ikm = [0x0bu8; 22];
        let salt = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
        let okm = hkdf_sha256(&salt, &ikm, &info, 42);
        assert_eq!(
            hex_encode(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865"
        );
    }

    #[test]
    fn derive_is_stable_for_the_same_account() {
        let salt = salt(7);
        let identity = DeviceIdentity::derive(&salt, Region::Cn, "uid-1");
        assert_eq!(
            identity.machine_id(),
            DeviceIdentity::derive(&salt, Region::Cn, "uid-1").machine_id()
        );
        assert_eq!(
            identity.session_id(),
            DeviceIdentity::derive(&salt, Region::Cn, "uid-1").session_id()
        );
        // 每请求标识同 nonce 同值（可复现），换 nonce 换值（不重复）。
        assert_eq!(identity.request_id(11), identity.request_id(11));
        assert_ne!(identity.request_id(11), identity.request_id(12));
    }

    #[test]
    fn derive_isolates_accounts_realms_and_labels() {
        let base_salt = salt(7);
        let cn = DeviceIdentity::derive(&base_salt, Region::Cn, "uid-1");
        let other_account = DeviceIdentity::derive(&base_salt, Region::Cn, "uid-2");
        let other_realm = DeviceIdentity::derive(&base_salt, Region::Global, "uid-1");
        let other_salt = DeviceIdentity::derive(&salt(9), Region::Cn, "uid-1");

        assert_ne!(cn.machine_id(), other_account.machine_id());
        assert_ne!(cn.machine_id(), other_realm.machine_id());
        assert_ne!(cn.machine_id(), other_salt.machine_id());
        // 同一账号的 machine / session / request 三个标签必须互不相等（否则上游一眼看穿）。
        assert_ne!(cn.machine_id(), cn.session_id());
        assert_ne!(cn.machine_id(), cn.request_id(0));
        assert_ne!(cn.session_id(), cn.request_id(0));
    }

    #[test]
    fn derived_values_have_uuid_v4_shape() {
        let salt = salt(3);
        let identity = DeviceIdentity::derive(&salt, Region::Cn, "uid-1");
        let request_id = identity.request_id(5);
        for value in [
            identity.machine_id(),
            identity.session_id(),
            request_id.as_str(),
        ] {
            assert_eq!(value.len(), 36, "{value}");
            let parsed = uuid::Uuid::parse_str(value).expect("uuid");
            assert_eq!(parsed.get_version_num(), 4, "{value}");
            assert_eq!(value.chars().filter(|c| *c == '-').count(), 4);
        }
    }

    #[test]
    fn identity_headers_are_the_single_mounting_point() {
        let salt = salt(3);
        let identity = DeviceIdentity::derive(&salt, Region::Cn, "uid-1");
        let headers = identity.headers(99);
        assert_eq!(headers[HEADER_MACHINE_ID], identity.machine_id());
        assert_eq!(headers[HEADER_SESSION_ID], identity.session_id());
        assert_eq!(headers[HEADER_REQUEST_ID], identity.request_id(99));
        assert_eq!(headers.len(), 3);
    }

    #[test]
    fn apply_is_a_noop_until_the_account_opts_in() {
        let salt = salt(3);
        let mut headers = HashMap::new();
        // 缺省（无 automation 字段）= 关：一个头都不加，与今日行为一致。
        assert!(!apply_with_salt(&salt, &json!({"uid": "uid-1"}), Region::Cn, 1, &mut headers));
        assert!(headers.is_empty());
        // 显式 false = 关。
        assert!(!apply_with_salt(
            &salt,
            &json!({"uid": "uid-1", "automation": {"device_identity": false}}),
            Region::Cn,
            1,
            &mut headers
        ));
        assert!(headers.is_empty());
        // 开了才写，且三个头齐全。
        assert!(apply_with_salt(&salt, &opt_in("uid-1"), Region::Cn, 1, &mut headers));
        assert_eq!(headers.len(), 3);
    }

    #[test]
    fn apply_is_idempotent_and_swallows_the_switch_being_turned_off() {
        let salt = salt(3);
        let mut headers = HashMap::new();
        assert!(apply_with_salt(&salt, &opt_in("uid-1"), Region::Cn, 1, &mut headers));
        let first = headers.clone();
        assert!(apply_with_salt(&salt, &opt_in("uid-1"), Region::Cn, 1, &mut headers));
        assert_eq!(headers, first);
        // 关掉后**新的一轮**不再附加（调用方从空 map 起算，绝无残留）。
        let mut next_round = HashMap::new();
        assert!(!apply_with_salt(
            &salt,
            &json!({"uid": "uid-1", "automation": {"device_identity": false}}),
            Region::Cn,
            2,
            &mut next_round
        ));
        assert!(next_round.is_empty());
    }

    #[test]
    fn apply_skips_accounts_without_a_uid() {
        let salt = salt(3);
        let mut headers = HashMap::new();
        assert!(!apply_with_salt(&salt, &opt_in(""), Region::Cn, 1, &mut headers));
        assert!(headers.is_empty());
    }

    #[test]
    fn mp_envelope_is_stable_per_uid_and_nick() {
        let salt = salt(5);
        let envelope = MpEnvelope::derive(&salt, Region::Cn, "uid-1", "nick-a");
        assert_eq!(
            envelope.device_id(),
            MpEnvelope::derive(&salt, Region::Cn, "uid-1", "nick-a").device_id()
        );
        assert_eq!(
            envelope.session_id(),
            MpEnvelope::derive(&salt, Region::Cn, "uid-1", "nick-a").session_id()
        );
        // 改昵称：设备不变，会话变。
        let renamed = MpEnvelope::derive(&salt, Region::Cn, "uid-1", "nick-b");
        assert_eq!(envelope.device_id(), renamed.device_id());
        assert_ne!(envelope.session_id(), renamed.session_id());
        // 换账号：两个都变。
        let other = MpEnvelope::derive(&salt, Region::Cn, "uid-2", "nick-a");
        assert_ne!(envelope.device_id(), other.device_id());
        assert_ne!(envelope.session_id(), other.session_id());
    }

    #[test]
    fn mp_apply_respects_the_same_opt_in_gate() {
        let salt = salt(5);
        let mut headers = HashMap::new();
        assert!(!apply_mp_with_salt(
            &salt,
            &json!({"uid": "uid-1"}),
            Region::Cn,
            "n",
            &mut headers
        ));
        assert!(headers.is_empty());
        assert!(apply_mp_with_salt(&salt, &opt_in("uid-1"), Region::Cn, "n", &mut headers));
        assert_eq!(headers.len(), 2);
        assert_eq!(
            headers[HEADER_MP_SESSION_ID],
            MpEnvelope::derive(&salt, Region::Cn, "uid-1", "n").session_id()
        );
    }

    // -----------------------------------------------------------------------
    // 安装盐的落盘纪律
    // -----------------------------------------------------------------------

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("wb-device-identity-{tag}-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("device_identity_salt.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn salt_is_created_once_and_then_reused() {
        let dir = TempDir::new("create");
        let store = SaltStore::at(dir.file());
        assert!(store.load().expect("load").is_none(), "首次运行前应为空");
        let first = store.load_or_create().expect("create");
        let second = store.load_or_create().expect("reload");
        assert_eq!(first, second, "第二次必须复用同一份盐");
        // 重新开一个 store（模拟进程重启）也必须拿到同一份。
        assert_eq!(SaltStore::at(dir.file()).load_or_create().expect("reload"), first);
        // 盐长度为 32 字节且非全零。
        assert_eq!(first.len(), SALT_LEN);
        assert_ne!(first, [0u8; SALT_LEN]);
    }

    #[cfg(unix)]
    #[test]
    fn salt_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new("perms");
        let store = SaltStore::at(dir.file());
        store.load_or_create().expect("create");
        let mode = std::fs::metadata(dir.file()).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "盐文件必须是 0600（at rest）");
    }

    #[test]
    fn corrupt_salt_file_fails_closed_instead_of_rotating() {
        let dir = TempDir::new("corrupt");
        let store = SaltStore::at(dir.file());
        std::fs::write(dir.file(), "{\"version\":1,\"salt\":\"zzzz\"}").expect("write");
        assert_eq!(
            store.load().expect_err("invalid hex must error"),
            "device identity salt: invalid hex"
        );
        assert!(store.load_or_create().is_err(), "损坏时绝不能静默换盐");
        // 文件原样保留（没有偷偷覆盖）。
        assert_eq!(
            std::fs::read_to_string(dir.file()).expect("read"),
            "{\"version\":1,\"salt\":\"zzzz\"}"
        );
    }

    #[test]
    fn salt_with_wrong_length_fails_closed() {
        let dir = TempDir::new("short");
        let store = SaltStore::at(dir.file());
        std::fs::write(dir.file(), "{\"version\":1,\"salt\":\"00112233\"}").expect("write");
        assert_eq!(
            store.load().expect_err("short salt must error"),
            "device identity salt: wrong length"
        );
    }

    // -----------------------------------------------------------------------
    // 红线：盐 / uid / 派生值一律不进日志与错误串
    // -----------------------------------------------------------------------

    #[test]
    fn debug_and_errors_never_leak_the_salt_uid_or_derived_values() {
        let salt = salt(0xab);
        let uid = "uid-secret-1138";
        let identity = DeviceIdentity::derive(&salt, Region::Cn, uid);
        let rendered = format!("{identity:?}");
        assert!(!rendered.contains(uid), "{rendered}");
        assert!(!rendered.contains(identity.machine_id()), "{rendered}");
        assert!(!rendered.contains(identity.session_id()), "{rendered}");
        assert!(rendered.contains(REDACTED));

        let envelope = MpEnvelope::derive(&salt, Region::Cn, uid, "nick-secret");
        let rendered = format!("{envelope:?}");
        assert!(!rendered.contains("nick-secret"), "{rendered}");
        assert!(!rendered.contains(envelope.device_id()), "{rendered}");
        assert!(rendered.contains(REDACTED));

        // 错误串只描述类别：不含盐的十六进制、不含 uid、不含派生值。
        let dir = TempDir::new("redact");
        let store = SaltStore::at(dir.file());
        let salt_hex = hex_encode(&salt);
        std::fs::write(dir.file(), format!("{{\"version\":1,\"salt\":\"{salt_hex}zz\"}}"))
            .expect("write");
        let error = store.load().expect_err("error");
        assert!(!error.contains(&salt_hex), "{error}");
        assert!(!error.contains(uid), "{error}");
        assert!(!error.contains(identity.machine_id()), "{error}");
    }
}
