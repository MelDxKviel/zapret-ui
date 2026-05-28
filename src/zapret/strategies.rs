//! Built-in zapret2 strategies.
//!
//! Replaces the Flowseal `.bat`-parsing catalog (`batparse.rs` +
//! `catalog.rs`) that the pre-`zapret-2` history used. Strategies here are
//! authored as Rust constants whose `build` function turns the live install
//! directory into a ready-to-run argv for `winws2.exe`. The argv values are
//! grounded in the upstream examples shipped in `bol-van/zapret-win-bundle`'s
//! `zapret-winws/preset2_example.cmd` and `preset2_wireguard.cmd`.
//!
//! Design notes:
//! - Argv is rebuilt on demand from the install dir each time the orchestrator
//!   asks the catalog for a strategy, so a service install (which copies the
//!   tree to `%ProgramData%\zapret-ui\zapret`) and a user-process install
//!   (under `%APPDATA%`) get correctly-rooted paths without us caching either.
//! - Paths are absolute. The bat files use `%~dp0…` relative to the script;
//!   we set the CWD to the install root when spawning, but absolute paths
//!   remove any ambiguity if a process inherits a different working dir.
//! - Strategy ids are `<short>-v2`. The `-v2` suffix is deliberate: it
//!   prevents an old config whose `last_strategy` referenced a Flowseal name
//!   (e.g. `"general (ALT2)"`) from accidentally matching a new one.
//! - `required_files` lists everything the argv references — Lua scripts,
//!   hostlists, raw windivert filter parts, blobs. [`check_required_files`]
//!   uses this list to emit a single, actionable warning when something is
//!   missing from the install (rather than letting winws2 die with a
//!   cryptic error mid-startup).

use std::path::{Path, PathBuf};

use crate::contracts::{Category, Strategy};
use crate::ports::StrategyCatalog;

/// One built-in strategy. The `build` fn is invoked lazily per resolution
/// — never cache its output; the install dir may differ between calls
/// (user vs. service-protected dir).
pub struct StrategyDef {
    /// Stable id stored in the config (`last_strategy`, `favorites`).
    pub id: &'static str,
    /// Human-friendly display name. Phase 11 will replace these with i18n
    /// keys once the UI is rewired to call `I18n.t` on strategy names.
    pub display_name: &'static str,
    pub description: &'static str,
    pub category: Category,
    /// Files (relative to the install root) the produced argv references.
    /// Used by [`check_required_files`] for an early "missing input"
    /// warning, and surfaced as `Strategy::requires_lists` for the UI.
    pub required_files: &'static [&'static str],
    /// Resolves the install dir into a complete `winws2.exe` argv.
    pub build: fn(&Path) -> Vec<String>,
}

/// The curated zapret2 strategy set. Order is the UI display order
/// (favorites pinned to the top by the catalog layer in the future).
pub fn builtin_strategies() -> &'static [StrategyDef] {
    &BUILTIN
}

static BUILTIN: [StrategyDef; 8] = [
    StrategyDef {
        id: "general-v2",
        display_name: "General (v2)",
        description: "Balanced default — covers HTTP, TLS, QUIC, Discord, STUN and WireGuard in one chain.",
        category: Category::Mixed,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "files/list-youtube.txt",
            "files/quic_initial_www_google_com.bin",
            "windivert.filter/windivert_part.discord_media.txt",
            "windivert.filter/windivert_part.stun.txt",
            "windivert.filter/windivert_part.wireguard.txt",
            "windivert.filter/windivert_part.quic_initial_ietf.txt",
        ],
        build: build_general_v2,
    },
    StrategyDef {
        id: "general-aggressive-v2",
        display_name: "General · aggressive (v2)",
        description: "Same coverage as General, with longer fake-packet repeat counts and a tcp_seq trick — try when the default isn't enough.",
        category: Category::Mixed,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "files/list-youtube.txt",
            "files/quic_initial_www_google_com.bin",
            "windivert.filter/windivert_part.discord_media.txt",
            "windivert.filter/windivert_part.stun.txt",
            "windivert.filter/windivert_part.wireguard.txt",
            "windivert.filter/windivert_part.quic_initial_ietf.txt",
        ],
        build: build_general_aggressive_v2,
    },
    StrategyDef {
        id: "general-light-v2",
        display_name: "General · light (v2)",
        description: "TLS-only minimal desync — skips HTTP/QUIC/WireGuard. Try when General breaks unrelated sites.",
        category: Category::Mixed,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
        ],
        build: build_general_light_v2,
    },
    StrategyDef {
        id: "auto-v2",
        display_name: "Auto (v2)",
        description: "Loads zapret-auto.lua + applies a hostlist-less TLS/QUIC desync. Works on any blocked TLS host, not just YouTube.",
        category: Category::Mixed,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "lua/zapret-auto.lua",
            "files/quic_initial_www_google_com.bin",
            "windivert.filter/windivert_part.quic_initial_ietf.txt",
        ],
        build: build_auto_v2,
    },
    StrategyDef {
        id: "youtube-tls-v2",
        display_name: "YouTube TLS (v2)",
        description: "Minimal TCP/443 TLS desync targeted at the YouTube hostlist.",
        category: Category::Youtube,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "files/list-youtube.txt",
        ],
        build: build_youtube_tls_v2,
    },
    StrategyDef {
        id: "youtube-quic-v2",
        display_name: "YouTube QUIC (v2)",
        description: "UDP/443 QUIC desync targeted at the YouTube hostlist.",
        category: Category::Youtube,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "files/list-youtube.txt",
            "files/quic_initial_www_google_com.bin",
            "windivert.filter/windivert_part.quic_initial_ietf.txt",
        ],
        build: build_youtube_quic_v2,
    },
    StrategyDef {
        id: "discord-v2",
        display_name: "Discord / VoIP (v2)",
        description: "Detects Discord, STUN and WireGuard by L7 protocol — no hostlist needed.",
        category: Category::Discord,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "windivert.filter/windivert_part.discord_media.txt",
            "windivert.filter/windivert_part.stun.txt",
            "windivert.filter/windivert_part.wireguard.txt",
        ],
        build: build_discord_v2,
    },
    StrategyDef {
        id: "wireguard-v2",
        display_name: "WireGuard (v2)",
        description: "Pure WireGuard initiation fix — ported from preset2_wireguard.cmd.",
        category: Category::Discord,
        required_files: &[
            "lua/zapret-lib.lua",
            "lua/zapret-antidpi.lua",
            "lua/zapret-auto.lua",
            "windivert.filter/windivert_part.wireguard.txt",
        ],
        build: build_wireguard_v2,
    },
];

// ─── path helpers ─────────────────────────────────────────────────────────

fn lua_init_file(install: &Path, name: &str) -> String {
    format!("--lua-init=@{}", install.join("lua").join(name).display())
}
fn blob(install: &Path, name: &str, file: &str) -> String {
    format!(
        "--blob={name}:@{}",
        install.join("files").join(file).display()
    )
}
fn raw_part(install: &Path, name: &str) -> String {
    format!(
        "--wf-raw-part=@{}",
        install.join("windivert.filter").join(name).display()
    )
}
fn hostlist(install: &Path, name: &str) -> String {
    format!(
        "--hostlist={}",
        install.join("files").join(name).display()
    )
}

// ─── strategy builders ────────────────────────────────────────────────────

/// Verbatim port of `zapret-winws/preset2_example.cmd`. Carries six
/// `--new`-separated profiles covering HTTP/TLS/QUIC and a Discord/STUN/
/// WireGuard catch-all. Default pick if the user doesn't run the tester.
fn build_general_v2(install: &Path) -> Vec<String> {
    let mut args = vec![
        // Global windivert filter — bring TCP 80/443 OUT into winws's view.
        "--wf-tcp-out=80,443".into(),
        // Helper library + the attack catalog, then tweak the default TLS
        // fake to randomize hostname (sni) on every connection.
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        "--lua-init=fake_default_tls = tls_mod(fake_default_tls,'rnd,rndsni')".into(),
        // Cached QUIC initial used by the YouTube-targeted fake profile.
        blob(install, "quic_google", "quic_initial_www_google_com.bin"),
        // Raw filter additions: catch traffic the standard `--wf-*` flags miss.
        raw_part(install, "windivert_part.discord_media.txt"),
        raw_part(install, "windivert_part.stun.txt"),
        raw_part(install, "windivert_part.wireguard.txt"),
        raw_part(install, "windivert_part.quic_initial_ietf.txt"),
        // ── Profile 1: plain HTTP on TCP/80 ──────────────────────────────
        "--filter-tcp=80".into(),
        "--filter-l7=http".into(),
        "--out-range=-d10".into(),
        "--payload=http_req".into(),
        "--lua-desync=fake:blob=fake_default_http:ip_autottl=-2,3-20:ip6_autottl=-2,3-20:tcp_md5".into(),
        "--lua-desync=fakedsplit:ip_autottl=-2,3-20:ip6_autottl=-2,3-20:tcp_md5".into(),
        "--new".into(),
        // ── Profile 2: TLS on TCP/443, YouTube-hostlist-scoped fake ──────
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        hostlist(install, "list-youtube.txt"),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:repeats=11:tls_mod=rnd,dupsid,sni=www.google.com".into(),
        "--lua-desync=multidisorder:pos=1,midsld".into(),
        "--new".into(),
        // ── Profile 3: TLS on TCP/443, generic (no hostlist) ────────────
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:tcp_seq=-10000:repeats=6".into(),
        "--lua-desync=multidisorder:pos=midsld".into(),
        "--new".into(),
        // ── Profile 4: QUIC on UDP/443, YouTube-hostlist-scoped fake ────
        "--filter-udp=443".into(),
        "--filter-l7=quic".into(),
        hostlist(install, "list-youtube.txt"),
        "--payload=quic_initial".into(),
        "--lua-desync=fake:blob=quic_google:repeats=11".into(),
        "--new".into(),
        // ── Profile 5: QUIC on UDP/443, generic ─────────────────────────
        "--filter-udp=443".into(),
        "--filter-l7=quic".into(),
        "--payload=quic_initial".into(),
        "--lua-desync=fake:blob=fake_default_quic:repeats=11".into(),
        "--new".into(),
        // ── Profile 6: WireGuard / STUN / Discord catch-all ─────────────
        "--filter-l7=wireguard,stun,discord".into(),
        "--payload=wireguard_initiation,wireguard_cookie,stun,discord_ip_discovery".into(),
        "--lua-desync=fake:blob=0x00000000000000000000000000000000:repeats=2".into(),
    ];
    args.shrink_to_fit();
    args
}

/// Same coverage as `general-v2` but with the fake-packet counts dialled up
/// (`repeats=20`) and an extra `tcp_seq` displacement on the TLS profile.
/// Pick when the default doesn't budge — some DPIs only count short bursts
/// of fakes, so cranking the repeats forces them past their cutoff.
fn build_general_aggressive_v2(install: &Path) -> Vec<String> {
    let mut args = vec![
        "--wf-tcp-out=80,443".into(),
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        "--lua-init=fake_default_tls = tls_mod(fake_default_tls,'rnd,rndsni')".into(),
        blob(install, "quic_google", "quic_initial_www_google_com.bin"),
        raw_part(install, "windivert_part.discord_media.txt"),
        raw_part(install, "windivert_part.stun.txt"),
        raw_part(install, "windivert_part.wireguard.txt"),
        raw_part(install, "windivert_part.quic_initial_ietf.txt"),
        // HTTP — same as general but with 2x repeats on the fake.
        "--filter-tcp=80".into(),
        "--filter-l7=http".into(),
        "--out-range=-d10".into(),
        "--payload=http_req".into(),
        "--lua-desync=fake:blob=fake_default_http:ip_autottl=-2,3-20:ip6_autottl=-2,3-20:tcp_md5:repeats=4".into(),
        "--lua-desync=fakedsplit:ip_autottl=-2,3-20:ip6_autottl=-2,3-20:tcp_md5".into(),
        "--new".into(),
        // TLS YouTube — repeats=20, tcp_seq displacement.
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        hostlist(install, "list-youtube.txt"),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:tcp_seq=-20000:repeats=20:tls_mod=rnd,dupsid,sni=www.google.com".into(),
        "--lua-desync=multidisorder:pos=2,midsld".into(),
        "--new".into(),
        // TLS generic — repeats=12.
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:tcp_seq=-20000:repeats=12".into(),
        "--lua-desync=multidisorder:pos=2,midsld".into(),
        "--new".into(),
        // QUIC YouTube — repeats=20.
        "--filter-udp=443".into(),
        "--filter-l7=quic".into(),
        hostlist(install, "list-youtube.txt"),
        "--payload=quic_initial".into(),
        "--lua-desync=fake:blob=quic_google:repeats=20".into(),
        "--new".into(),
        // QUIC generic — repeats=20.
        "--filter-udp=443".into(),
        "--filter-l7=quic".into(),
        "--payload=quic_initial".into(),
        "--lua-desync=fake:blob=fake_default_quic:repeats=20".into(),
        "--new".into(),
        // WireGuard/STUN/Discord catch-all — repeats=4.
        "--filter-l7=wireguard,stun,discord".into(),
        "--payload=wireguard_initiation,wireguard_cookie,stun,discord_ip_discovery".into(),
        "--lua-desync=fake:blob=0x00000000000000000000000000000000:repeats=4".into(),
    ];
    args.shrink_to_fit();
    args
}

/// TLS-only minimal desync. Skips HTTP, QUIC, WireGuard, STUN and Discord
/// — useful when General drops connections to unrelated sites because of
/// the aggressive catch-all profile. Costs less CPU too.
fn build_general_light_v2(install: &Path) -> Vec<String> {
    vec![
        "--wf-tcp-out=443".into(),
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        "--lua-init=fake_default_tls = tls_mod(fake_default_tls,'rnd,rndsni')".into(),
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:repeats=6".into(),
        "--lua-desync=multidisorder:pos=midsld".into(),
    ]
}

/// Hostlist-less TLS+QUIC desync with the `zapret-auto.lua` automation
/// library loaded. Loading `zapret-auto.lua` only registers the
/// `automate_*` helpers — the desync chain here doesn't call them — but
/// it leaves the door open to adaptive per-host strategies in the future
/// (the orchestration layer is already in place). For the user, the
/// practical win is that this preset isn't tied to the YouTube hostlist
/// so it works on any blocked TLS site.
fn build_auto_v2(install: &Path) -> Vec<String> {
    vec![
        "--wf-tcp-out=443".into(),
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        lua_init_file(install, "zapret-auto.lua"),
        "--lua-init=fake_default_tls = tls_mod(fake_default_tls,'rnd,rndsni')".into(),
        blob(install, "quic_google", "quic_initial_www_google_com.bin"),
        raw_part(install, "windivert_part.quic_initial_ietf.txt"),
        // TLS (generic, no hostlist) — covers any blocked TLS host.
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:repeats=11:tls_mod=rnd,dupsid,sni=www.google.com".into(),
        "--lua-desync=multidisorder:pos=1,midsld".into(),
        "--new".into(),
        // QUIC (generic).
        "--filter-udp=443".into(),
        "--filter-l7=quic".into(),
        "--payload=quic_initial".into(),
        "--lua-desync=fake:blob=quic_google:repeats=11".into(),
    ]
}

/// Minimal TCP/443 TLS desync constrained to the YouTube hostlist. Useful
/// when general-v2 is too aggressive (e.g. breaks an unrelated TLS site).
fn build_youtube_tls_v2(install: &Path) -> Vec<String> {
    vec![
        "--wf-tcp-out=443".into(),
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        "--lua-init=fake_default_tls = tls_mod(fake_default_tls,'rnd,rndsni')".into(),
        "--filter-tcp=443".into(),
        "--filter-l7=tls".into(),
        hostlist(install, "list-youtube.txt"),
        "--out-range=-d10".into(),
        "--payload=tls_client_hello".into(),
        "--lua-desync=fake:blob=fake_default_tls:tcp_md5:repeats=11:tls_mod=rnd,dupsid,sni=www.google.com".into(),
        "--lua-desync=multidisorder:pos=1,midsld".into(),
    ]
}

/// UDP/443 QUIC desync constrained to the YouTube hostlist. Pair with
/// youtube-tls-v2 when the browser uses HTTP/3 (most modern Chromes).
fn build_youtube_quic_v2(install: &Path) -> Vec<String> {
    vec![
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        blob(install, "quic_google", "quic_initial_www_google_com.bin"),
        raw_part(install, "windivert_part.quic_initial_ietf.txt"),
        "--filter-udp=443".into(),
        "--filter-l7=quic".into(),
        hostlist(install, "list-youtube.txt"),
        "--payload=quic_initial".into(),
        "--lua-desync=fake:blob=quic_google:repeats=11".into(),
    ]
}

/// Detect Discord/STUN/WireGuard via L7 protocol — no hostlist needed,
/// works on any IP the protocol parser recognises.
fn build_discord_v2(install: &Path) -> Vec<String> {
    vec![
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        raw_part(install, "windivert_part.discord_media.txt"),
        raw_part(install, "windivert_part.stun.txt"),
        raw_part(install, "windivert_part.wireguard.txt"),
        "--filter-l7=wireguard,stun,discord".into(),
        "--payload=wireguard_initiation,wireguard_cookie,stun,discord_ip_discovery".into(),
        "--lua-desync=fake:blob=0x00000000000000000000000000000000:repeats=2".into(),
    ]
}

/// Verbatim port of `zapret-winws/preset2_wireguard.cmd`. Targets the
/// WireGuard initiation handshake with the auto Lua helper.
fn build_wireguard_v2(install: &Path) -> Vec<String> {
    vec![
        raw_part(install, "windivert_part.wireguard.txt"),
        lua_init_file(install, "zapret-lib.lua"),
        lua_init_file(install, "zapret-antidpi.lua"),
        lua_init_file(install, "zapret-auto.lua"),
        "--filter-l7=wireguard".into(),
        "--payload=wireguard_initiation".into(),
        "--lua-desync=repeater:instances=2:repeats=3".into(),
        "--lua-desync=luaexec:code=desync.rnd=brandom(math.random(32,64))".into(),
        "--lua-desync=fake:blob=rnd".into(),
    ]
}

// ─── catalog ─────────────────────────────────────────────────────────────

/// Lookup-friendly façade over [`builtin_strategies`] that the orchestrator
/// interacts with through the `StrategyCatalog` trait.
pub struct BuiltinCatalog {
    install_dir: PathBuf,
}

impl BuiltinCatalog {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }
}

impl StrategyCatalog for BuiltinCatalog {
    fn all(&self) -> Vec<Strategy> {
        builtin_strategies()
            .iter()
            .map(|def| materialize(def, &self.install_dir))
            .collect()
    }

    fn by_id(&self, id: &str) -> Option<Strategy> {
        builtin_strategies()
            .iter()
            .find(|d| d.id == id)
            .map(|d| materialize(d, &self.install_dir))
    }

    fn by_category(&self, c: Category) -> Vec<Strategy> {
        builtin_strategies()
            .iter()
            .filter(|d| d.category == c)
            .map(|d| materialize(d, &self.install_dir))
            .collect()
    }
}

fn materialize(def: &StrategyDef, install_dir: &Path) -> Strategy {
    Strategy {
        id: def.id.to_string(),
        display_name: def.display_name.to_string(),
        category: def.category,
        description: def.description.to_string(),
        winws_args: (def.build)(install_dir),
        requires_lists: def.required_files.iter().map(|s| s.to_string()).collect(),
    }
}

// ─── runtime sanity check ────────────────────────────────────────────────

/// Returns the (relative) paths a strategy needs that are missing from the
/// install. The orchestrator calls this just before spawning winws2 so a
/// missing-file warning lands in the log *before* the cryptic winws2 error,
/// and the UI can surface a "reinstall the bundle" hint instead of leaving
/// the user staring at an unexplained crash.
pub fn check_required_files(install_dir: &Path, strategy: &Strategy) -> Vec<String> {
    strategy
        .requires_lists
        .iter()
        .filter(|rel| !install_dir.join(rel).exists())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every builtin must yield a non-empty argv whose every element starts
    /// with `--` (or, for the `--new` separator, equals `--new`). Catches
    /// `format!` typos that drop a leading `--`.
    #[test]
    fn every_builtin_builds_a_non_empty_argv() {
        let dir = PathBuf::from(r"C:\install");
        for def in builtin_strategies() {
            let argv = (def.build)(&dir);
            assert!(!argv.is_empty(), "{} produced empty argv", def.id);
            for (i, a) in argv.iter().enumerate() {
                assert!(
                    a.starts_with("--"),
                    "{} argv[{i}] {a:?} does not start with --",
                    def.id
                );
                assert!(!a.contains('\0'), "{} argv[{i}] contains a NUL byte", def.id);
                assert!(!a.contains('%'), "{} argv[{i}] has unresolved %var%", def.id);
            }
        }
    }

    /// Strategy ids are stored in user config (`last_strategy`, `favorites`),
    /// so they must be unique. A duplicate id would let two strategies fight
    /// over the same lookup.
    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for def in builtin_strategies() {
            assert!(seen.insert(def.id), "duplicate id {}", def.id);
        }
    }

    #[test]
    fn required_files_are_safe_relative_paths() {
        for def in builtin_strategies() {
            for rel in def.required_files {
                assert!(!rel.starts_with('/'), "{} {rel} is absolute", def.id);
                assert!(!rel.contains(".."), "{} {rel} escapes the install dir", def.id);
                assert!(!rel.is_empty(), "{} has an empty required_files entry", def.id);
            }
        }
    }

    #[test]
    fn catalog_by_id_finds_each_builtin() {
        let catalog = BuiltinCatalog::new(PathBuf::from(r"C:\install"));
        for def in builtin_strategies() {
            let s = catalog.by_id(def.id).expect("by_id finds builtin");
            assert_eq!(s.id, def.id);
        }
        assert!(catalog.by_id("nope").is_none());
    }

    #[test]
    fn check_required_files_reports_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path();
        let catalog = BuiltinCatalog::new(install.to_path_buf());
        let general = catalog.by_id("general-v2").unwrap();
        let missing = check_required_files(install, &general);
        // Empty install → everything required is missing.
        assert_eq!(missing.len(), general.requires_lists.len());

        // Drop one file in and confirm it disappears from the missing set.
        std::fs::create_dir_all(install.join("lua")).unwrap();
        std::fs::write(install.join("lua").join("zapret-lib.lua"), b"-- stub").unwrap();
        let missing2 = check_required_files(install, &general);
        assert!(!missing2.iter().any(|m| m == "lua/zapret-lib.lua"));
        assert_eq!(missing2.len(), general.requires_lists.len() - 1);
    }
}
