//! 打一张 **local vs golden** 的 TLS ClientHello 对账表。
//!
//! ```bash
//! cargo run -p buddy-switch-core --example tls_probe
//! ```
//!
//! - golden：参考基线 `wb2api@22b7966` 的 uTLS 桥（`tools/wb-tls-bridge/main.go`）。
//! - local：本仓 `reqwest` + `rustls` 在当前依赖版本下**实际发出**的握手首包，
//!   由 `modules::tls_parity::capture_local_client_hello` 在本机 loopback 抓取（不连外网）。
//!
//! 结论（差异表与「为什么不挂 uTLS 侧车」）写在 `docs/specs/WB-AUTOMATION-ARCH.md` §7.5。
//!
//! 这个 example 是**只读诊断**：不发业务请求、不写任何文件。

use buddy_switch_core::modules::tls_parity::{
    capture_local_client_hello, cipher_name, compare, extension_name, golden, measured,
    render_table, same_shape, Profile, Verdict,
};

fn print_profile(title: &str, profile: &Profile) {
    println!("== {title} ==");
    println!(
        "ciphers ({}): {}",
        profile.ciphers.len(),
        profile
            .ciphers
            .iter()
            .map(|id| format!("{id:#06x} {}", cipher_name(*id)))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!(
        "extensions ({}): {}",
        profile.extensions.len(),
        profile
            .extensions
            .iter()
            .map(|id| format!("{id:#06x} {}", extension_name(*id)))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!(
        "groups: {}",
        profile
            .groups
            .iter()
            .map(|id| format!("{id:#06x}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "sig_algs: {}",
        profile
            .sig_algs
            .iter()
            .map(|id| format!("{id:#06x}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "supported_versions: {}",
        profile
            .supported_versions
            .iter()
            .map(|id| format!("{id:#06x}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "key_share_groups: {}",
        profile
            .key_share_groups
            .iter()
            .map(|id| format!("{id:#06x}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "alpn: [{}] {}",
        profile.alpn.len(),
        if profile.alpn.is_empty() {
            "（无）".to_string()
        } else {
            profile.alpn.join(",")
        }
    );
    println!("sni: {}", profile.has_sni);
    println!("grease: {}", profile.has_grease);
    println!();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("TLS ClientHello 对账（golden = wb2api@22b7966 的 uTLS 桥，local = 本仓实测）\n");
    print_profile("golden", &golden());

    let live = match capture_local_client_hello().await {
        Ok(profile) => profile,
        Err(err) => {
            println!("!! 抓包失败：{err}");
            println!("!! 只用编译期记录的实测常量出表（可能与当前依赖不一致）");
            measured()
        }
    };
    print_profile("local（本次实测）", &live);

    // 扩展顺序每个连接都随机（rustls 的抗指纹特性），所以只比「除顺序外」的形状。
    if !same_shape(&live, &measured()) {
        println!("!! 本次实测与 MEASURED_* 常量不一致——依赖升级后先看下面的表，再改常量与 spec §7.5\n");
    }

    println!("{}", render_table(&live));

    let deltas = compare(&live);
    let unmatched: Vec<_> = deltas.iter().filter(|d| d.verdict != Verdict::Match).collect();
    println!(
        "结论：{} / {} 个字段与 golden 不一致；其中 rustls 表达不出来的有 {} 个。",
        unmatched.len(),
        deltas.len(),
        unmatched
            .iter()
            .filter(|d| d.verdict == Verdict::CannotExpress)
            .count()
    );
    println!("决策与理由见 docs/specs/WB-AUTOMATION-ARCH.md §7.5（本轮：narrowing，不挂 uTLS 侧车）。");
}
