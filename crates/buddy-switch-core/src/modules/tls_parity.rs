//! TLS 指纹对账：把参考实现的 **golden ClientHello** 与本仓实际会发出的 ClientHello 摆在一起比。
//!
//! 依据：`docs/specs/WB-AUTOMATION-ARCH.md` §7.5；参考基线 `wb2api@22b7966` 的
//! `tools/wb-tls-bridge/main.go`（`officialHelloSpec`）与同名 `README.md`。
//!
//! 三件事，别混：
//!
//! 1. **golden 是抄来的数据，不是我们希望的东西**：18 套件（顺序有意义）、10 个扩展（顺序有意义）、
//!    `X25519 / P-256 / P-384`、9 个签名算法、**没有 ALPN**、TLS1.3→TLS1.2。抄写纪律见
//!    [`GOLDEN_CIPHER_SUITES`] 上方的注释（`README` 只说了个数，具体 id 必须回 Go 源核对）。
//! 2. **local 是量出来的，不是猜的**：[`capture_local_client_hello`] 在 loopback 上抓一次真实的
//!    握手首包（只读不写，握手必然失败，这是刻意的），再用 [`parse_client_hello`] 解出来。
//!    因此「本仓 TLS 到底长什么样」有可重复的证据，而不是靠读 rustls 源码猜。
//! 3. **结论写在 spec 里，代码只负责让它不可悄悄漂移**：[`compare`] 出的是差异表，
//!    [`DELTA_FIELDS`] 是**本节唯一允许的差异清单**——rustls/reqwest 一旦换了默认栈，
//!    单测立刻红，强制先改 spec 再改这里。
//!
//! 为什么不挂 uTLS 侧车：见 spec §7.5 的决策段（引入外部运行时需要**单独批准**）。
//! 本模块**不发任何真实请求**：抓包目标是 `localhost` 上的自建监听器。

use std::time::Duration;

/// golden 密套件（**顺序即 ClientHello 里的顺序**），照抄 Go 源 `officialCipherSuites`。
///
/// ⚠️ 抄写纪律：`README.md` 只承诺「18 ciphers、10 extensions、no ALPN、X25519/P-256/P-384」，
/// 具体 id **必须**回 `main.go:25` 逐个核对；`golden_profile_is_the_audited_node_hello`
/// 钉死个数与顺序，防止「顺手排序」。
pub const GOLDEN_CIPHER_SUITES: [u16; 18] = [
    0x1301, // TLS_AES_128_GCM_SHA256
    0x1302, // TLS_AES_256_GCM_SHA384
    0x1303, // TLS_CHACHA20_POLY1305_SHA256
    0xc02f, // TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
    0xc02b, // TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
    0xc030, // TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
    0xc02c, // TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
    0xc027, // TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256
    0xcca9, // TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
    0xcca8, // TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
    0xc009, // TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA
    0xc013, // TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA
    0xc00a, // TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA
    0xc014, // TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA
    0x009c, // TLS_RSA_WITH_AES_128_GCM_SHA256
    0x009d, // TLS_RSA_WITH_AES_256_GCM_SHA384
    0x002f, // TLS_RSA_WITH_AES_128_CBC_SHA
    0x0035, // TLS_RSA_WITH_AES_256_CBC_SHA
];

/// golden 扩展（**顺序即 ClientHello 里的顺序**），照抄 Go 源 `officialHelloSpec().Extensions`。
pub const GOLDEN_EXTENSIONS: [u16; 10] = [
    0x0000, // server_name（SNI）
    0x0017, // extended_master_secret
    0xff01, // renegotiation_info（`RenegotiateOnceAsClient`）
    0x000a, // supported_groups
    0x000b, // ec_point_formats（`{0}` = uncompressed）
    0x0023, // session_ticket
    0x000d, // signature_algorithms
    0x0033, // key_share（只有 X25519）
    0x002d, // psk_key_exchange_modes（`psk_dhe_ke`）
    0x002b, // supported_versions（TLS1.3, TLS1.2）
];

/// golden 支持组（`SupportedCurvesExtension`）。
pub const GOLDEN_SUPPORTED_GROUPS: [u16; 3] = [0x001d, 0x0017, 0x0018]; // X25519, P-256, P-384

/// golden 签名算法（`officialSigAlgs`，顺序照抄）。
pub const GOLDEN_SIGNATURE_ALGORITHMS: [u16; 9] = [
    0x0403, // ecdsa_secp256r1_sha256
    0x0804, // rsa_pss_rsae_sha256
    0x0401, // rsa_pkcs1_sha256
    0x0503, // ecdsa_secp384r1_sha384
    0x0805, // rsa_pss_rsae_sha384
    0x0501, // rsa_pkcs1_sha384
    0x0806, // rsa_pss_rsae_sha512
    0x0601, // rsa_pkcs1_sha512
    0x0201, // rsa_pkcs1_sha1
];

/// golden 支持的版本（TLS1.3 在前）。
pub const GOLDEN_SUPPORTED_VERSIONS: [u16; 2] = [0x0304, 0x0303];

/// golden 的 key_share 组（只有 X25519）。
pub const GOLDEN_KEY_SHARE_GROUPS: [u16; 1] = [0x001d];

/// golden **没有 ALPN**（这是它与浏览器/rustls 默认栈最容易被忽略的一条差异）。
pub const GOLDEN_ALPN: [&str; 0] = [];

/// golden 的压缩方法（只有 `null`）。
pub const GOLDEN_COMPRESSION: [u8; 1] = [0];

/// golden Hello 是否带 SNI。
pub const GOLDEN_HAS_SNI: bool = true;

/// 本仓**实测**的 ClientHello 形状（2026-11-16，`reqwest 0.12.28` + `rustls 0.23.43` + `ring`），
/// 由 [`capture_local_client_hello`] 在 loopback 抓首包解出；复现：
/// `cargo run -p buddy-switch-core --example tls_probe`。
///
/// 作用只有一个：**漂移告警**。依赖升级后抓包结果若与它不同，
/// `live_client_hello_equals_the_measured_profile` 会红——先跑 example 看新形状，
/// 确认 spec §7.5 的差异表是否仍然成立，再改这里的常量。
// 10 条 AEAD 套件 + `TLS_EMPTY_RENEGOTIATION_INFO_SCSV`（0x00ff 是信令值，不是真套件）。
pub const MEASURED_CIPHER_SUITES: [u16; 10] = [
    0x1302, 0x1301, 0x1303, 0xc02c, 0xc02b, 0xcca9, 0xc030, 0xc02f, 0xcca8, 0x00ff,
];
// ⚠️ 这里是**集合（按 id 升序）**，不是线上顺序：rustls 0.23 每个连接都会重新打乱
// ClientHello 的扩展顺序（抗指纹特性），实测四次得到四种不同顺序——见
// `extension_order_is_randomized_per_connection`。所以「扩展顺序对齐 golden」在本仓
// 不只是做不到，而是**每一轮本来就不一样**。
// 另外一个反直觉的实测结论：**0x000b（ec_point_formats）是发的**，
// 与「rustls 从不发这个扩展」的常见印象相反——这正是要量、而不是读源码猜的理由。
pub const MEASURED_EXTENSIONS: [u16; 11] = [
    0x0000, 0x0005, 0x000a, 0x000b, 0x000d, 0x0010, 0x0017, 0x0023, 0x002b, 0x002d, 0x0033,
];
// 支持组**与 golden 完全一致**（X25519 / P-256 / P-384）。
pub const MEASURED_SUPPORTED_GROUPS: [u16; 3] = [0x001d, 0x0017, 0x0018];
// 多一个 rsa_pss_pss_sha256(0x0807)，少一个 rsa_pkcs1_sha1(0x0201)。
pub const MEASURED_SIGNATURE_ALGORITHMS: [u16; 9] = [
    0x0503, 0x0403, 0x0807, 0x0806, 0x0805, 0x0804, 0x0601, 0x0501, 0x0401,
];
pub const MEASURED_SUPPORTED_VERSIONS: [u16; 2] = [0x0304, 0x0303];
pub const MEASURED_KEY_SHARE_GROUPS: [u16; 1] = [0x001d];
/// `reqwest` 在没有 `http2` 特性时只写 `http/1.1`；golden 是**完全不带 ALPN**。
pub const MEASURED_ALPN: [&str; 1] = ["http/1.1"];
pub const MEASURED_COMPRESSION: [u8; 1] = [0];
pub const MEASURED_HAS_SNI: bool = true;

/// 本仓**实测**形状（见 [`MEASURED_CIPHER_SUITES`] 的说明）。
pub fn measured() -> Profile {
    Profile {
        ciphers: MEASURED_CIPHER_SUITES.to_vec(),
        extensions: MEASURED_EXTENSIONS.to_vec(),
        groups: MEASURED_SUPPORTED_GROUPS.to_vec(),
        sig_algs: MEASURED_SIGNATURE_ALGORITHMS.to_vec(),
        supported_versions: MEASURED_SUPPORTED_VERSIONS.to_vec(),
        key_share_groups: MEASURED_KEY_SHARE_GROUPS.to_vec(),
        alpn: MEASURED_ALPN.iter().map(|s| s.to_string()).collect(),
        compression: MEASURED_COMPRESSION.to_vec(),
        has_sni: MEASURED_HAS_SNI,
        has_grease: false,
    }
}

/// spec §7.5 差异表里**唯一允许**出现的字段名（顺序无关）。多一条或少一条都要先改 spec。
pub const DELTA_FIELDS: [&str; 4] = [
    "cipher_suites",
    "extensions",
    "signature_algorithms",
    "alpn",
];

/// 解析出来（或写死）的一个 ClientHello 形状。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Profile {
    pub ciphers: Vec<u16>,
    pub extensions: Vec<u16>,
    pub groups: Vec<u16>,
    pub sig_algs: Vec<u16>,
    pub supported_versions: Vec<u16>,
    pub key_share_groups: Vec<u16>,
    pub alpn: Vec<String>,
    pub compression: Vec<u8>,
    pub has_sni: bool,
    /// 是否出现 GREASE 值（`0x0a0a` 形态）。参考实现**不用** GREASE，这里只用作证据。
    pub has_grease: bool,
}

/// golden 形状（由上面那些常量拼出来；测试与 [`compare`] 都用它）。
pub fn golden() -> Profile {
    Profile {
        ciphers: GOLDEN_CIPHER_SUITES.to_vec(),
        extensions: GOLDEN_EXTENSIONS.to_vec(),
        groups: GOLDEN_SUPPORTED_GROUPS.to_vec(),
        sig_algs: GOLDEN_SIGNATURE_ALGORITHMS.to_vec(),
        supported_versions: GOLDEN_SUPPORTED_VERSIONS.to_vec(),
        key_share_groups: GOLDEN_KEY_SHARE_GROUPS.to_vec(),
        alpn: GOLDEN_ALPN.iter().map(|s| s.to_string()).collect(),
        compression: GOLDEN_COMPRESSION.to_vec(),
        has_sni: GOLDEN_HAS_SNI,
        has_grease: false,
    }
}

// ---------------------------------------------------------------------------
// 解析：只吃 ClientHello（握手首包），不试图做完整 TLS
// ---------------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.remaining() < n {
            return Err(format!(
                "ClientHello 截断：需要 {n} 字节，只剩 {}",
                self.remaining()
            ));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u24(&mut self) -> Result<usize, String> {
        let b = self.take(3)?;
        Ok(((b[0] as usize) << 16) | ((b[1] as usize) << 8) | b[2] as usize)
    }
}

fn be16s(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect()
}

/// GREASE 值：`0x0a0a`、`0x1a1a`、…、`0xfafa`（两字节相同且低半字节为 `a`）。
pub fn is_grease(value: u16) -> bool {
    let [hi, lo] = value.to_be_bytes();
    hi == lo && (hi & 0x0f) == 0x0a
}

/// 解析一条完整的 TLS 记录（`type(1) version(2) length(2) body`），其 body 必须是 ClientHello。
///
/// 只接受 `content_type = 0x16`（handshake）且 `handshake_type = 0x01`；其它一律报错，
/// 避免把 ServerHello / 应用数据误当指纹。
pub fn parse_client_hello(bytes: &[u8]) -> Result<Profile, String> {
    let mut r = Reader::new(bytes);
    let content_type = r.u8()?;
    if content_type != 0x16 {
        return Err(format!("记录类型 {content_type:#04x} 不是 handshake（0x16）"));
    }
    let _record_version = r.u16()?;
    let record_len = r.u16()? as usize;
    let record = r.take(record_len)?;

    let mut hs = Reader::new(record);
    let hs_type = hs.u8()?;
    if hs_type != 0x01 {
        return Err(format!("握手类型 {hs_type:#04x} 不是 ClientHello（0x01）"));
    }
    let hs_len = hs.u24()?;
    let mut body = Reader::new(hs.take(hs_len)?);

    let _legacy_version = body.u16()?;
    body.take(32)?; // random
    let session_id_len = body.u8()? as usize;
    body.take(session_id_len)?;
    let ciphers_len = body.u16()? as usize;
    let ciphers = be16s(body.take(ciphers_len)?);
    let compression_len = body.u8()? as usize;
    let compression = body.take(compression_len)?.to_vec();

    let mut profile = Profile {
        ciphers,
        compression,
        ..Default::default()
    };
    if body.remaining() == 0 {
        return Ok(profile);
    }

    let extensions_len = body.u16()? as usize;
    let mut ext = Reader::new(body.take(extensions_len)?);
    while ext.remaining() >= 4 {
        let id = ext.u16()?;
        let len = ext.u16()? as usize;
        let data = ext.take(len)?;
        profile.extensions.push(id);
        match id {
            // server_name
            0x0000 => {
                let mut d = Reader::new(data);
                let total = d.u16()? as usize;
                let mut names = Reader::new(d.take(total)?);
                if names.remaining() >= 3 {
                    let _name_type = names.u8()?;
                    let name_len = names.u16()? as usize;
                    let name = names.take(name_len)?;
                    profile.has_sni = !name.is_empty();
                }
            }
            // supported_groups
            0x000a => {
                let mut d = Reader::new(data);
                let total = d.u16()? as usize;
                profile.groups = be16s(d.take(total)?);
            }
            // signature_algorithms
            0x000d => {
                let mut d = Reader::new(data);
                let total = d.u16()? as usize;
                profile.sig_algs = be16s(d.take(total)?);
            }
            // ALPN
            0x0010 => {
                let mut d = Reader::new(data);
                let total = d.u16()? as usize;
                let mut list = Reader::new(d.take(total)?);
                while list.remaining() >= 1 {
                    let name_len = list.u8()? as usize;
                    let name = list.take(name_len)?;
                    profile
                        .alpn
                        .push(String::from_utf8_lossy(name).into_owned());
                }
            }
            // supported_versions（客户端形态是 1 字节长度）
            0x002b => {
                let mut d = Reader::new(data);
                let total = d.u8()? as usize;
                profile.supported_versions = be16s(d.take(total)?);
            }
            // key_share（客户端形态）
            0x0033 => {
                let mut d = Reader::new(data);
                let total = d.u16()? as usize;
                let mut shares = Reader::new(d.take(total)?);
                while shares.remaining() >= 4 {
                    let group = shares.u16()?;
                    let key_len = shares.u16()? as usize;
                    shares.take(key_len)?;
                    profile.key_share_groups.push(group);
                }
            }
            _ => {}
        }
    }

    profile.has_grease = profile
        .ciphers
        .iter()
        .chain(profile.extensions.iter())
        .chain(profile.groups.iter())
        .copied()
        .any(is_grease);

    Ok(profile)
}

// ---------------------------------------------------------------------------
// 抓一次真实的 ClientHello（**只连 loopback**）
// ---------------------------------------------------------------------------

/// 在本机 loopback 起一个只读监听器，让 `reqwest` 去连它，抓回握手首包并解析。
///
/// - **不发业务请求**：监听器读完 ClientHello 立刻关闭，握手注定失败（客户端那边返回错误，
///   这里刻意忽略）。所以本函数可在离线 CI 里跑，不依赖任何外部网络。
/// - 用 `localhost`（而不是 `127.0.0.1`）作 URL：rustls 对 IP 字面量**不发 SNI**，
///   而 golden 带 SNI——用 IP 会凭空多出一条「SNI 缺失」的假差异。
/// - 监听线程用 `std`（阻塞 `accept`/`read`），客户端用异步：避免引入 tokio 的 `io-util` 特性。
pub async fn capture_local_client_hello() -> Result<Profile, String> {
    use std::io::Read;

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("无法绑定 loopback 监听器：{e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("读不到监听器地址：{e}"))?
        .port();

    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
    let handle = std::thread::spawn(move || {
        let record = (|| -> Result<Vec<u8>, String> {
            let (mut sock, _) = listener.accept().map_err(|e| format!("accept 失败：{e}"))?;
            sock.set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| format!("设置读超时失败：{e}"))?;
            let mut header = [0u8; 5];
            sock.read_exact(&mut header)
                .map_err(|e| format!("读记录头失败：{e}"))?;
            let len = u16::from_be_bytes([header[3], header[4]]) as usize;
            let mut body = vec![0u8; len];
            sock.read_exact(&mut body)
                .map_err(|e| format!("读记录体（{len} 字节）失败：{e}"))?;
            let mut record = header.to_vec();
            record.extend_from_slice(&body);
            Ok(record)
        })();
        let _ = tx.send(record);
    });

    let url = format!("https://localhost:{port}/");
    if let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        // 握手不会完成：监听器读完首包就关闭连接，这里失败是预期结果。
        let _ = client.get(&url).send().await;
    }

    let record = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|e| format!("没抓到 ClientHello：{e}"))??;
    let _ = handle.join();
    parse_client_hello(&record)
}

// ---------------------------------------------------------------------------
// 差异表
// ---------------------------------------------------------------------------

/// 一条字段级差异的结论。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// 一致。
    Match,
    /// 不一致，但**本轮接受**（已写进 spec §7.5 的差异表）。
    Accepted,
    /// 不一致且**rustls/reqwest 表达不出来**（要么实现里没有，要么需要新依赖/外部运行时）。
    CannotExpress,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Accepted => "accepted",
            Self::CannotExpress => "cannot-express",
        }
    }
}

/// 一行差异。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delta {
    pub field: &'static str,
    pub golden: String,
    pub local: String,
    pub verdict: Verdict,
    pub why: &'static str,
}

fn hex_list(values: &[u16]) -> String {
    values
        .iter()
        .map(|v| format!("{v:#06x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 按 id 升序（用于**顺序不稳定**的字段：rustls 会打乱扩展顺序）。
fn sorted(list: &[u16]) -> Vec<u16> {
    let mut out = list.to_vec();
    out.sort_unstable();
    out
}

fn fields(list: &[u16]) -> String {
    format!("[{}] {}", list.len(), hex_list(list))
}

/// 逐字段比对 golden 与 local，返回**全部**字段（含一致项，供 example 打整张表）。
///
/// `why` 是「为什么这个差异今天不动」的一句话，直接对应 spec §7.5 的差异表；
/// 改结论请先改 spec，再改这里（`delta_fields_are_the_ones_recorded_in_the_spec` 会拦）。
pub fn compare(local: &Profile) -> Vec<Delta> {
    let golden = golden();
    let mut rows: Vec<Delta> = Vec::new();

    macro_rules! row {
        ($field:literal, $equal:expr, $golden:expr, $local:expr, $verdict:expr, $why:literal) => {{
            let equal: bool = $equal;
            rows.push(Delta {
                field: $field,
                golden: $golden,
                local: $local,
                verdict: if equal { Verdict::Match } else { $verdict },
                why: if equal { "一致" } else { $why },
            });
        }};
    }

    row!(
        "cipher_suites",
        golden.ciphers == local.ciphers,
        fields(&golden.ciphers),
        fields(&local.ciphers),
        Verdict::CannotExpress,
        "rustls 只有 10 条 AEAD 套件 + SCSV(0x00ff)；golden 的 7 条 CBC、2 条 RSA-kx 不存在，顺序也不可配"
    );
    row!(
        "extensions",
        sorted(&golden.extensions) == sorted(&local.extensions),
        fields(&sorted(&golden.extensions)),
        fields(&sorted(&local.extensions)),
        Verdict::CannotExpress,
        "rustls 每个连接都打乱扩展顺序（抗指纹），集合也不同：多发 status_request(0x0005)/ALPN(0x0010)，不发 renegotiation_info(0xff01)"
    );
    row!(
        "supported_groups",
        golden.groups == local.groups,
        fields(&golden.groups),
        fields(&local.groups),
        Verdict::CannotExpress,
        "rustls 的 KX group 列表不对外可配（只有 provider 固定集）"
    );
    row!(
        "signature_algorithms",
        golden.sig_algs == local.sig_algs,
        fields(&golden.sig_algs),
        fields(&local.sig_algs),
        Verdict::CannotExpress,
        "顺序不同；多一个 rsa_pss_pss_sha256(0x0807)，少一个 rsa_pkcs1_sha1(0x0201)"
    );
    row!(
        "key_share_groups",
        golden.key_share_groups == local.key_share_groups,
        fields(&golden.key_share_groups),
        fields(&local.key_share_groups),
        Verdict::CannotExpress,
        "首包 key_share 由 provider 的默认组决定，不可注入"
    );
    row!(
        "alpn",
        golden.alpn == local.alpn,
        format!("[{}] {}", golden.alpn.len(), golden.alpn.join(",")),
        format!("[{}] {}", local.alpn.len(), local.alpn.join(",")),
        Verdict::Accepted,
        "reqwest 自行写入 ALPN；要清空得用 use_preconfigured_tls（需直接依赖 rustls）"
    );
    row!(
        "supported_versions",
        golden.supported_versions == local.supported_versions,
        fields(&golden.supported_versions),
        fields(&local.supported_versions),
        Verdict::Accepted,
        "版本集合一致即可"
    );
    row!(
        "compression",
        golden.compression == local.compression,
        format!("{:?}", golden.compression),
        format!("{:?}", local.compression),
        Verdict::Accepted,
        "两边都只有 null"
    );
    row!(
        "server_name",
        golden.has_sni == local.has_sni,
        golden.has_sni.to_string(),
        local.has_sni.to_string(),
        Verdict::Accepted,
        "有 SNI 就够；具体主机名随请求变化，本就不该固定"
    );
    rows
}

/// 两个形状是否「同一套 TLS 配置」：除**扩展顺序**外逐字段严格相等。
///
/// 扩展顺序必须排除——rustls 每个连接都重排（见
/// `extension_order_is_randomized_per_connection`），拿它做相等判断只会得到假差异。
pub fn same_shape(a: &Profile, b: &Profile) -> bool {
    a.ciphers == b.ciphers
        && sorted(&a.extensions) == sorted(&b.extensions)
        && a.groups == b.groups
        && a.sig_algs == b.sig_algs
        && a.supported_versions == b.supported_versions
        && a.key_share_groups == b.key_share_groups
        && a.alpn == b.alpn
        && a.compression == b.compression
        && a.has_sni == b.has_sni
        && a.has_grease == b.has_grease
}

/// 差异表里**当前真正不一致**的字段名（供测试与 spec §7.5 对账）。
pub fn delta_fields(local: &Profile) -> Vec<&'static str> {
    compare(local)
        .into_iter()
        .filter(|d| d.verdict != Verdict::Match)
        .map(|d| d.field)
        .collect()
}

/// 渲染成 `field | verdict | golden | local` 的文本表，并在末尾列出差异原因（example 用）。
pub fn render_table(local: &Profile) -> String {
    let rows = compare(local);
    let mut out = String::new();
    out.push_str("field                 | verdict        | golden                                                       | local\n");
    out.push_str("----------------------+----------------+--------------------------------------------------------------+--------------------------------------------------------------\n");
    for r in &rows {
        out.push_str(&format!(
            "{:<21} | {:<14} | {:<60} | {}\n",
            r.field,
            r.verdict.as_str(),
            r.golden,
            r.local
        ));
    }
    out.push('\n');
    for r in rows.iter().filter(|r| r.verdict != Verdict::Match) {
        out.push_str(&format!("- {}: {}\n", r.field, r.why));
    }
    out
}

/// 套件名（未知返回 `unknown`）。
pub fn cipher_name(id: u16) -> &'static str {
    match id {
        0x1301 => "TLS_AES_128_GCM_SHA256",
        0x1302 => "TLS_AES_256_GCM_SHA384",
        0x1303 => "TLS_CHACHA20_POLY1305_SHA256",
        0xc02f => "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
        0xc02b => "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
        0xc030 => "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
        0xc02c => "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
        0xc027 => "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
        0xcca9 => "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
        0xcca8 => "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
        0xc009 => "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA",
        0xc013 => "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA",
        0xc00a => "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA",
        0xc014 => "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA",
        0x009c => "TLS_RSA_WITH_AES_128_GCM_SHA256",
        0x009d => "TLS_RSA_WITH_AES_256_GCM_SHA384",
        0x002f => "TLS_RSA_WITH_AES_128_CBC_SHA",
        0x0035 => "TLS_RSA_WITH_AES_256_CBC_SHA",
        0x00ff => "TLS_EMPTY_RENEGOTIATION_INFO_SCSV",
        _ => "unknown",
    }
}

/// 扩展名（未知返回 `unknown`）。
pub fn extension_name(id: u16) -> &'static str {
    match id {
        0x0000 => "server_name",
        0x0005 => "status_request",
        0x000a => "supported_groups",
        0x000b => "ec_point_formats",
        0x000d => "signature_algorithms",
        0x0010 => "application_layer_protocol_negotiation",
        0x0017 => "extended_master_secret",
        0x001b => "certificate_authorities",
        0x0023 => "session_ticket",
        0x002b => "supported_versions",
        0x002d => "psk_key_exchange_modes",
        0x0033 => "key_share",
        0xff01 => "renegotiation_info",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// golden 就是审计里那一条：18 套件 / 10 扩展 / 无 ALPN / 三组曲线。
    ///
    /// 这条测试的唯一职责是**拦住转录漂移**（比如有人顺手把套件排序、或把 `0x000b` 当成
    /// 「多余」删掉）。它不证明参考实现的线上行为，只证明我们抄对了 `main.go:25` 那张表。
    #[test]
    fn golden_profile_is_the_audited_node_hello() {
        let g = golden();
        assert_eq!(g.ciphers.len(), 18, "`officialCipherSuites` 是 18 条");
        assert_eq!(g.extensions.len(), 10, "`officialHelloSpec` 是 10 个扩展");
        assert!(g.alpn.is_empty(), "审计口径：无 ALPN");
        assert_eq!(g.groups, vec![0x001d, 0x0017, 0x0018], "X25519 / P-256 / P-384");
        assert_eq!(g.sig_algs.len(), 9, "`officialSigAlgs` 是 9 条");
        assert_eq!(g.key_share_groups, vec![0x001d], "首包只带 X25519 的 key_share");
        assert_eq!(g.supported_versions, vec![0x0304, 0x0303], "TLS1.3 在前");
        assert!(g.has_sni);
        assert_eq!(g.compression, vec![0]);
        assert!(!g.has_grease, "参考实现不用 GREASE");
        // 顺序钉死：前三条是 TLS1.3 套件，与 Go 源一致。
        assert_eq!(&g.ciphers[..3], &[0x1301, 0x1302, 0x1303]);
        // 扩展顺序钉死：SNI 第一、supported_versions 最后。
        assert_eq!(g.extensions.first().copied(), Some(0x0000));
        assert_eq!(g.extensions.last().copied(), Some(0x002b));
        // 18 条里没有重复，也没有 GREASE。
        let mut sorted = g.ciphers.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 18, "golden 套件列表里有重复项");
        assert!(!g.ciphers.iter().copied().any(is_grease));
    }

    /// 解析器必须能把「golden 形状」原样读回来：用 golden 数据自己拼一条 ClientHello 记录。
    ///
    /// 这条让 [`parse_client_hello`] 的正确性不依赖网络与依赖版本。
    #[test]
    fn parser_round_trips_a_hand_built_client_hello() {
        let g = golden();
        let record = build_client_hello(&g, "www.workbuddy.cn");
        let parsed = parse_client_hello(&record).expect("golden 记录必须能解析");
        assert_eq!(parsed.ciphers, g.ciphers);
        assert_eq!(parsed.extensions, g.extensions);
        assert_eq!(parsed.groups, g.groups);
        assert_eq!(parsed.sig_algs, g.sig_algs);
        assert_eq!(parsed.supported_versions, g.supported_versions);
        assert_eq!(parsed.key_share_groups, g.key_share_groups);
        assert!(parsed.alpn.is_empty());
        assert!(parsed.has_sni);
        assert_eq!(parsed.compression, vec![0]);
        assert!(!parsed.has_grease);
    }

    #[test]
    fn parser_rejects_non_handshake_and_truncated_records() {
        let record = build_client_hello(&golden(), "example.com");
        // 非 handshake 记录类型（0x17 = application_data）。
        let mut wrong_type = record.clone();
        wrong_type[0] = 0x17;
        assert!(parse_client_hello(&wrong_type).is_err());
        // 截断。
        assert!(parse_client_hello(&record[..record.len() - 3]).is_err());
        assert!(parse_client_hello(&[]).is_err());
    }

    /// 抓包必须能解出来，且与 [`MEASURED_CIPHER_SUITES`] 一致。
    ///
    /// 依赖升级后这里会红——那时先跑 `cargo run -p buddy-switch-core --example tls_probe`
    /// 看新形状，再决定是改常量还是改 spec §7.5 的差异表。
    #[tokio::test]
    async fn live_client_hello_equals_the_measured_profile() {
        let live = capture_local_client_hello()
            .await
            .expect("loopback 抓包失败（本测试只连 127.0.0.1，不连外网）");
        let want = measured();
        assert_eq!(live.ciphers, want.ciphers, "密套件列表变了（依赖升级？）");
        assert_eq!(
            sorted(&live.extensions),
            sorted(&want.extensions),
            "扩展集合变了（顺序本来就每个连接随机，见 extension_order_is_randomized_per_connection）"
        );
        assert_eq!(live.groups, want.groups, "支持组变了");
        assert_eq!(live.sig_algs, want.sig_algs, "签名算法变了");
        assert_eq!(live.supported_versions, want.supported_versions);
        assert_eq!(live.key_share_groups, want.key_share_groups);
        assert_eq!(live.alpn, want.alpn, "ALPN 变了");
        assert_eq!(live.has_sni, want.has_sni);
        assert!(!live.has_grease, "rustls 不应该发 GREASE");
    }

    /// rustls **每个连接都重新打乱扩展顺序**（0.23 的抗指纹特性）——这条不是猜的，是抓出来的。
    ///
    /// 它对本节的意义：即使集合能对上，「扩展顺序逐字对齐 golden」也**不可能**在本仓成立，
    /// 而且本仓自己两次握手的顺序就不一样。哪天这条红了，说明 rustls 改了策略，
    /// spec §7.5 的差异表必须跟着重写。
    #[tokio::test]
    async fn extension_order_is_randomized_per_connection() {
        let mut orders: Vec<Vec<u16>> = Vec::new();
        for _ in 0..6 {
            let live = capture_local_client_hello()
                .await
                .expect("loopback 抓包失败（本测试只连 127.0.0.1，不连外网）");
            assert_eq!(
                sorted(&live.extensions),
                sorted(&MEASURED_EXTENSIONS),
                "扩展集合变了，顺序随机性无从谈起"
            );
            orders.push(live.extensions);
        }
        let mut distinct = orders.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "6 次抓包的扩展顺序完全相同——rustls 不再打乱顺序了？先更新 spec §7.5"
        );
    }

    /// 差异字段必须正好是 spec §7.5 记过的那几条。
    ///
    /// 用实测形状而不是现场抓包的形状，是为了在没网/无监听权限的机器上也能跑；
    /// 现场形状由 `live_client_hello_equals_the_measured_profile` 保证与它一致。
    #[test]
    fn delta_fields_are_the_ones_recorded_in_the_spec() {
        let mut got = delta_fields(&measured());
        got.sort_unstable();
        let mut want = DELTA_FIELDS.to_vec();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "实测形状与 golden 的差异字段变了：先更新 spec §7.5 的差异表，再改 DELTA_FIELDS"
        );
    }

    /// 本仓**表达不出来**的那几条具体事实（写死成断言，防止有人以为「配一下就行」）。
    #[test]
    fn local_stack_cannot_express_the_golden_suites() {
        let live = measured();
        // rustls 没有 CBC 套件：golden 里那 7 条 CBC 一条都不可能出现。
        for id in [0xc027u16, 0xc009, 0xc013, 0xc00a, 0xc014, 0x002f, 0x0035] {
            assert!(
                !live.ciphers.contains(&id),
                "rustls 居然发了 CBC 套件 {id:#06x}？差异表需要重写"
            );
        }
        // RSA 密钥交换同样没有（rustls 只做 (EC)DHE）。
        for id in [0x009cu16, 0x009d] {
            assert!(!live.ciphers.contains(&id), "rustls 居然发了 RSA-kx 套件 {id:#06x}");
        }
        // SCSV 是 rustls 的固定附带项（不是可配套件）。
        assert!(
            live.ciphers.contains(&0x00ff),
            "SCSV(0x00ff) 不见了：reqwest/rustls 的默认栈变了"
        );
        // 扩展：rustls **会**发 ec_point_formats（实测纠正了「不发」的常见印象），
        // 但永远不发 renegotiation_info。
        assert!(live.extensions.contains(&0x000b), "rustls 现在不发 ec_point_formats 了？");
        assert!(!live.extensions.contains(&0xff01), "renegotiation_info 不该出现");
        // 支持组与 golden 完全一致——这是唯一「白捡」的字段，别让它悄悄漂移。
        assert_eq!(live.groups, GOLDEN_SUPPORTED_GROUPS.to_vec());
        assert_eq!(live.key_share_groups, GOLDEN_KEY_SHARE_GROUPS.to_vec());
        // 没有任何 GREASE。
        assert!(!live.ciphers.iter().copied().any(is_grease));
        assert!(!live.extensions.iter().copied().any(is_grease));
        assert!(!live.groups.iter().copied().any(is_grease));
    }

    /// 决定必须躺在 spec 里：本文件与 §7.5 说的必须是同一件事。
    ///
    /// 用 `include_str!` 把 spec 读进编译产物：谁改了代码不写文档、或删了决策段，都会红。
    #[test]
    fn decision_is_recorded_in_the_spec() {
        let spec = include_str!("../../../../docs/specs/WB-AUTOMATION-ARCH.md");
        for needle in [
            "### 7.5 TLS 指纹",
            "tls_parity.rs",
            "uTLS",
            "0x000b",
            "ALPN",
            "narrowing",
        ] {
            assert!(
                spec.contains(needle),
                "spec §7.5 缺了 `{needle}`：TLS 决策必须写在文档里，不能只在代码里"
            );
        }
    }

    // ------------------------------------------------------------------
    // 测试夹具：按给定形状拼一条 ClientHello 记录
    // ------------------------------------------------------------------

    fn push_u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u24(out: &mut Vec<u8>, v: usize) {
        out.push(((v >> 16) & 0xff) as u8);
        out.push(((v >> 8) & 0xff) as u8);
        out.push((v & 0xff) as u8);
    }

    fn build_client_hello(profile: &Profile, host: &str) -> Vec<u8> {
        let mut exts: Vec<u8> = Vec::new();
        for id in &profile.extensions {
            let body: Vec<u8> = match *id {
                0x0000 => {
                    let mut name = vec![0u8]; // host_name
                    push_u16(&mut name, host.len() as u16);
                    name.extend_from_slice(host.as_bytes());
                    let mut list = Vec::new();
                    push_u16(&mut list, name.len() as u16);
                    list.extend_from_slice(&name);
                    list
                }
                0x000a | 0x000d => {
                    let mut flat = Vec::new();
                    let values = if *id == 0x000a {
                        &profile.groups
                    } else {
                        &profile.sig_algs
                    };
                    for v in values {
                        push_u16(&mut flat, *v);
                    }
                    let mut b = Vec::new();
                    push_u16(&mut b, flat.len() as u16);
                    b.extend_from_slice(&flat);
                    b
                }
                0x000b => vec![1, 0], // 只支持 uncompressed
                0x0010 => {
                    let mut list = Vec::new();
                    for proto in &profile.alpn {
                        list.push(proto.len() as u8);
                        list.extend_from_slice(proto.as_bytes());
                    }
                    let mut b = Vec::new();
                    push_u16(&mut b, list.len() as u16);
                    b.extend_from_slice(&list);
                    b
                }
                0x002b => {
                    let mut b = vec![(profile.supported_versions.len() * 2) as u8];
                    for v in &profile.supported_versions {
                        push_u16(&mut b, *v);
                    }
                    b
                }
                0x0033 => {
                    let mut shares = Vec::new();
                    for g in &profile.key_share_groups {
                        push_u16(&mut shares, *g);
                        push_u16(&mut shares, 32); // 32 字节假公钥（只需结构合法）
                        shares.extend_from_slice(&[7u8; 32]);
                    }
                    let mut b = Vec::new();
                    push_u16(&mut b, shares.len() as u16);
                    b.extend_from_slice(&shares);
                    b
                }
                _ => Vec::new(),
            };
            push_u16(&mut exts, *id);
            push_u16(&mut exts, body.len() as u16);
            exts.extend_from_slice(&body);
        }

        let mut body: Vec<u8> = Vec::new();
        push_u16(&mut body, 0x0303); // legacy_version
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(32); // session_id 长度
        body.extend_from_slice(&[1u8; 32]);
        push_u16(&mut body, (profile.ciphers.len() * 2) as u16);
        for c in &profile.ciphers {
            push_u16(&mut body, *c);
        }
        body.push(profile.compression.len() as u8);
        body.extend_from_slice(&profile.compression);
        push_u16(&mut body, exts.len() as u16);
        body.extend_from_slice(&exts);

        let mut handshake = vec![0x01u8];
        push_u24(&mut handshake, body.len());
        handshake.extend_from_slice(&body);

        let mut record = vec![0x16u8];
        push_u16(&mut record, 0x0301);
        push_u16(&mut record, handshake.len() as u16);
        record.extend_from_slice(&handshake);
        record
    }
}
