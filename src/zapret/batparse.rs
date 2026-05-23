//! Parser for Flowseal's zapret-discord-youtube `.bat` strategy files.
//!
//! Each preset (general.bat, "general (ALT2).bat", ...) launches winws.exe with a long
//! argument list using batch variables (%BIN%, %LISTS%, %~dp0, %GameFilterTCP%, ...).
//! We replicate the substitution that service.bat performs so we can run winws.exe directly.

use std::path::Path;
use crate::contracts::{Strategy, Category};

/// When the game filter is disabled (default), service.bat sets these to "12".
const GAME_FILTER: &str = "12";

/// Create the user list files that `service.bat:load_user_lists` would create,
/// otherwise winws.exe aborts with "cannot access hostlist file".
pub fn ensure_user_lists(install_dir: &Path) {
    let lists = install_dir.join("lists");
    let _ = std::fs::create_dir_all(&lists);
    let defaults = [
        ("ipset-exclude-user.txt", "203.0.113.113/32\n"),
        ("list-general-user.txt", "domain.example.abc\n"),
        ("list-exclude-user.txt", "domain.example.abc\n"),
    ];
    for (name, content) in defaults {
        let p = lists.join(name);
        if !p.exists() {
            let _ = std::fs::write(&p, content);
        }
    }
}

/// Join the (possibly `^`-continued) winws.exe command line out of a .bat file
/// and return everything after `winws.exe"`.
fn extract_winws_command(content: &str) -> Option<String> {
    let mut capturing = false;
    let mut joined = String::new();
    for raw in content.lines() {
        let line = raw.trim_end();
        if !capturing {
            if let Some(idx) = line.to_lowercase().find("winws.exe") {
                // take the part after winws.exe (and a possible closing quote)
                let after = &line[idx + "winws.exe".len()..];
                let after = after.strip_prefix('"').unwrap_or(after);
                joined.push_str(after.trim_end_matches('^').trim());
                joined.push(' ');
                capturing = true;
                if !line.ends_with('^') {
                    break;
                }
            }
        } else {
            joined.push_str(line.trim_end_matches('^').trim());
            joined.push(' ');
            if !line.ends_with('^') {
                break;
            }
        }
    }
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Tokenize a command string by whitespace, treating double-quoted spans as part of a token.
fn tokenize(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_content = false;
    for ch in cmd.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_content = true; // keep empty-quoted tokens out, but mark seen
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_content {
                    tokens.push(std::mem::take(&mut cur));
                    has_content = false;
                }
            }
            c => {
                cur.push(c);
                has_content = true;
            }
        }
    }
    if has_content && !cur.is_empty() {
        tokens.push(cur);
    }
    tokens.retain(|t| !t.is_empty() && t != "^");
    tokens
}

/// Substitute batch variables with absolute paths / values.
fn substitute(token: &str, install_dir: &Path) -> String {
    let dp0 = format!("{}\\", install_dir.display());
    let bin = format!("{}\\bin\\", install_dir.display());
    let lists = format!("{}\\lists\\", install_dir.display());

    token
        .replace("%BIN%", &bin)
        .replace("%LISTS%", &lists)
        .replace("%~dp0", &dp0)
        .replace("%GameFilterTCP%", GAME_FILTER)
        .replace("%GameFilterUDP%", GAME_FILTER)
        .replace("%GameFilter%", GAME_FILTER)
}

/// Parse a .bat file's content into a ready-to-run winws argv (paths resolved).
pub fn parse_winws_args(content: &str, install_dir: &Path) -> Option<Vec<String>> {
    let cmd = extract_winws_command(content)?;
    let args: Vec<String> = tokenize(&cmd)
        .into_iter()
        .map(|t| substitute(&t, install_dir))
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

/// Hostlist files referenced by the args (for diagnostics).
pub fn referenced_lists(args: &[String]) -> Vec<String> {
    args.iter()
        .filter_map(|a| {
            for key in ["--hostlist=", "--ipset=", "--hostlist-exclude=", "--ipset-exclude="] {
                if let Some(v) = a.strip_prefix(key) {
                    return Some(v.to_string());
                }
            }
            None
        })
        .collect()
}

/// Guess a category from the preset file name.
pub fn category_for(name: &str) -> Category {
    let n = name.to_lowercase();
    if n.contains("discord") {
        Category::Discord
    } else if n.contains("youtube") || n.contains("yt") {
        Category::Youtube
    } else if n.contains("mgts") {
        Category::Mgts
    } else if n.contains("rostelecom") || n.contains("rt") {
        Category::Rostelecom
    } else if n.contains("mts") {
        Category::Mts
    } else if n.contains("beeline") {
        Category::Beeline
    } else {
        // The flowseal presets are general "mixed" Discord+YouTube bypass profiles.
        Category::Mixed
    }
}

/// Human-friendly description from the preset file name.
pub fn describe(name: &str) -> String {
    let upper = name.to_uppercase();
    if upper == "GENERAL" {
        "Default recommended profile (Discord + YouTube + general TCP/UDP).".to_string()
    } else if upper.contains("FAKE TLS AUTO") {
        "Auto fake-TLS variant. Try if the default profile is unstable.".to_string()
    } else if upper.contains("SIMPLE FAKE") {
        "Lightweight fake-packet variant.".to_string()
    } else if upper.starts_with("GENERAL (ALT") {
        "Alternative desync profile — try if the default does not work for your ISP.".to_string()
    } else {
        "Bypass profile from the zapret distribution.".to_string()
    }
}

/// Build a Strategy from a preset .bat file path.
pub fn strategy_from_bat(bat_path: &Path, install_dir: &Path) -> Option<Strategy> {
    let stem = bat_path.file_stem()?.to_string_lossy().to_string();
    if stem.eq_ignore_ascii_case("service") {
        return None;
    }
    let content = std::fs::read_to_string(bat_path).ok()?;
    let args = parse_winws_args(&content, install_dir)?;
    let lists = referenced_lists(&args);
    Some(Strategy {
        id: stem.clone(),
        display_name: stem.clone(),
        category: category_for(&stem),
        description: describe(&stem),
        winws_args: args,
        requires_lists: lists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Trimmed but faithful slice of the real general.bat winws invocation.
    const SAMPLE_BAT: &str = concat!(
        "@echo off\r\n",
        "set \"BIN=%~dp0bin\\\"\r\n",
        "set \"LISTS=%~dp0lists\\\"\r\n",
        "cd /d %BIN%\r\n",
        "start \"zapret: %~n0\" /min \"%BIN%winws.exe\" --wf-tcp=80,443,2053,%GameFilterTCP% --wf-udp=443,50000-50100,%GameFilterUDP% ^\r\n",
        "--filter-udp=443 --hostlist=\"%LISTS%list-general.txt\" --hostlist-exclude=\"%LISTS%list-exclude.txt\" --dpi-desync=fake --dpi-desync-repeats=6 --dpi-desync-fake-quic=\"%BIN%quic_initial_www_google_com.bin\" --new ^\r\n",
        "--filter-tcp=443 --hostlist=\"%LISTS%list-google.txt\" --dpi-desync=multisplit --dpi-desync-split-pos=1\r\n",
    );

    #[test]
    fn parses_real_general_bat() {
        let content = SAMPLE_BAT.to_string();
        let dir = PathBuf::from(r"C:\zap");
        let args = parse_winws_args(&content, &dir).expect("should parse");
        // No unresolved batch variables / stray quotes remain
        for a in &args {
            assert!(!a.contains('%'), "unresolved var in: {a}");
            assert!(!a.contains('"'), "stray quote in: {a}");
        }
        // First arg should be the --wf-tcp filter, game filter resolved to 12
        assert!(args[0].starts_with("--wf-tcp="), "first arg: {}", args[0]);
        assert!(args[0].ends_with(",12"), "game filter not resolved: {}", args[0]);
        // hostlist points at our absolute lists dir
        assert!(args.iter().any(|a| a.contains(r"C:\zap\lists\list-general.txt")),
            "no resolved list-general path; args={args:?}");
        // bin files resolved
        assert!(args.iter().any(|a| a.contains(r"C:\zap\bin\quic_initial_www_google_com.bin")),
            "no resolved bin path");
        // The start/min prefix must be gone
        assert!(!args.iter().any(|a| a.eq_ignore_ascii_case("/min") || a.contains("zapret:")),
            "start prefix leaked: {args:?}");
        println!("parsed {} args; sample: {:?}", args.len(), &args[..args.len().min(6)]);
    }
}
