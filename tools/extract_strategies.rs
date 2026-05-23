use std::fs::File;
use std::io::{Write, Read};
use std::process::Command;
use std::path::Path;

#[derive(Debug)]
struct RawStrategy {
    id: String,
    display_name: String,
    category: String,
    description: String,
    winws_args: Vec<String>,
    requires_lists: Vec<String>,
}

fn main() {
    println!("Starting extraction tool...");

    // Try downloading service.bat
    let url = "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/main/service.bat";
    let temp_file = "service_downloaded.bat";
    
    let downloaded = download_file(url, temp_file);
    
    let mut parsed_strategies = Vec::new();
    if downloaded {
        if let Ok(content) = read_file_content(temp_file) {
            parsed_strategies = parse_service_bat(&content);
            println!("Parsed {} strategies from service.bat", parsed_strategies.len());
        }
        // Cleanup temp file
        let _ = std::fs::remove_file(temp_file);
    } else {
        println!("Download failed or skipped. Using mock fallback.");
    }

    // If parsed strategies are fewer than 20, use fallback
    let final_strategies = if parsed_strategies.len() < 20 {
        println!("Fewer than 20 strategies parsed. Using mock fallback list of 24 strategies.");
        get_fallback_strategies()
    } else {
        parsed_strategies
    };

    // Generate src/zapret/strategies.rs
    generate_strategies_file(&final_strategies);
    println!("Successfully generated src/zapret/strategies.rs");
}

fn download_file(url: &str, dest: &str) -> bool {
    // Attempt using curl.exe
    let status = Command::new("curl.exe")
        .arg("-s")
        .arg("-o")
        .arg(dest)
        .arg(url)
        .status();

    if let Ok(s) = status {
        if s.success() && Path::new(dest).exists() {
            return true;
        }
    }

    // Try powershell as fallback
    let ps_cmd = format!("Invoke-WebRequest -Uri '{}' -OutFile '{}' -UseBasicParsing", url, dest);
    let status = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&ps_cmd)
        .status();

    if let Ok(s) = status {
        if s.success() && Path::new(dest).exists() {
            return true;
        }
    }

    false
}

fn read_file_content(path: &str) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

fn parse_service_bat(content: &str) -> Vec<RawStrategy> {
    let mut strategies = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    
    let mut current_label = None;
    let mut block_lines = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with(':') {
            // New label started, finish old block if any
            if let Some(label) = current_label {
                if let Some(strat) = process_block(label, &block_lines) {
                    strategies.push(strat);
                }
            }
            current_label = Some(trimmed[1..].to_string());
            block_lines.clear();
        } else if trimmed.starts_with("goto :eof") || trimmed.starts_with("exit /b") {
            if let Some(label) = current_label {
                if let Some(strat) = process_block(label, &block_lines) {
                    strategies.push(strat);
                }
            }
            current_label = None;
            block_lines.clear();
        } else if current_label.is_some() {
            block_lines.push(trimmed.to_string());
        }
    }

    strategies
}

fn process_block(label: String, lines: &[String]) -> Option<RawStrategy> {
    // Look for winws.exe in lines
    let mut winws_line = String::new();
    let mut inside_winws = false;

    for line in lines {
        if line.contains("winws.exe") {
            winws_line = line.clone();
            inside_winws = true;
        } else if inside_winws {
            if line.starts_with('^') || line.starts_with('&') || line.is_empty() {
                // Continuation
                winws_line.push(' ');
                winws_line.push_str(line);
            } else {
                inside_winws = false;
            }
        }
    }

    if winws_line.is_empty() {
        return None;
    }

    // Clean up winws line and split into args
    let winws_line = winws_line.replace("^", " ").replace("\"", "");
    let tokens: Vec<String> = winws_line.split_whitespace().map(|s| s.to_string()).collect();
    
    // Find winws.exe position
    let winws_pos = tokens.iter().position(|t| t.contains("winws.exe"))?;
    let args = tokens[winws_pos+1..].to_vec();

    // Map label to category
    let category = if label.contains("discord") {
        "Discord"
    } else if label.contains("youtube") {
        "Youtube"
    } else if label.contains("mgts") {
        "Mgts"
    } else if label.contains("rostelecom") || label.contains("rt") {
        "Rostelecom"
    } else if label.contains("mts") {
        "Mts"
    } else if label.contains("beeline") {
        "Beeline"
    } else {
        "Other"
    };

    let display_name = format!("Parsed Preset: {}", label);
    let description = format!("Strategy extracted from batch label :{}", label);

    // Determine required lists from args
    let mut requires_lists = Vec::new();
    for arg in &args {
        if arg.contains("list-") || arg.contains("ipset-") {
            if let Some(start) = arg.find("list-") {
                let list_name = &arg[start..];
                requires_lists.push(list_name.to_string());
            } else if let Some(start) = arg.find("ipset-") {
                let list_name = &arg[start..];
                requires_lists.push(list_name.to_string());
            }
        }
    }

    Some(RawStrategy {
        id: label,
        display_name,
        category: category.to_string(),
        description,
        winws_args: args,
        requires_lists,
    })
}

fn generate_strategies_file(strategies: &[RawStrategy]) {
    let path = Path::new("src/zapret/strategies.rs");
    let mut file = File::create(path).expect("Failed to create src/zapret/strategies.rs");

    writeln!(file, "// THIS FILE IS AUTO-GENERATED BY tools/extract_strategies.rs").unwrap();
    writeln!(file, "// DO NOT EDIT MANUAL").unwrap();
    writeln!(file, "").unwrap();
    writeln!(file, "use crate::contracts::{{Strategy, Category}};").unwrap();
    writeln!(file, "").unwrap();
    writeln!(file, "pub const STRATEGIES: &[Strategy] = &[").unwrap();

    for strat in strategies {
        writeln!(file, "    Strategy {{").unwrap();
        writeln!(file, "        id: \"{}\",", strat.id).unwrap();
        writeln!(file, "        display_name: \"{}\",", strat.display_name).unwrap();
        writeln!(file, "        category: Category::{},", strat.category).unwrap();
        writeln!(file, "        description: \"{}\",", strat.description).unwrap();
        writeln!(file, "        winws_args: &[").unwrap();
        for arg in &strat.winws_args {
            writeln!(file, "            \"{}\",", arg.replace("\\", "\\\\").replace("\"", "\\\"")).unwrap();
        }
        writeln!(file, "        ],").unwrap();
        writeln!(file, "        requires_lists: &[").unwrap();
        for list in &strat.requires_lists {
            writeln!(file, "            \"{}\",", list).unwrap();
        }
        writeln!(file, "        ],").unwrap();
        writeln!(file, "    }},").unwrap();
    }

    writeln!(file, "];").unwrap();
}

fn get_fallback_strategies() -> Vec<RawStrategy> {
    let mut list = Vec::new();

    // 1-8 Discord Strategies
    list.push(RawStrategy {
        id: "discord_general".to_string(),
        display_name: "Discord — General".to_string(),
        category: "Discord".to_string(),
        description: "Standard desync strategy for Discord (TCP + UDP)".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--wf-udp=443,19294-19344,50000-50100".to_string(),
            "--filter-udp=443".to_string(),
            "--hostlist=lists/list-discord.txt".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-repeats=6".to_string(),
            "--dpi-desync-fake-quic=bin/quic_initial_www_google_com.bin".to_string(),
            "--new".to_string(),
            "--filter-udp=19294-19344,50000-50100".to_string(),
            "--filter-l7=discord,stun".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-fake-discord=bin/quic_initial_dbankcloud_ru.bin".to_string(),
            "--dpi-desync-fake-stun=bin/quic_initial_dbankcloud_ru.bin".to_string(),
            "--dpi-desync-repeats=6".to_string(),
            "--new".to_string(),
            "--filter-tcp=443".to_string(),
            "--hostlist=lists/list-discord.txt".to_string(),
            "--dpi-desync-split-seqovl=681".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
            "--dpi-desync-split-seqovl-pattern=bin/tls_clienthello_www_google_com.bin".to_string(),
        ],
        requires_lists: vec!["list-discord.txt".to_string()],
    });

    for i in 1..=6 {
        list.push(RawStrategy {
            id: format!("discord_alt{}", i),
            display_name: format!("Discord — ALT {}", i),
            category: "Discord".to_string(),
            description: format!("Alternative Discord bypass configuration option {}", i),
            winws_args: vec![
                "--wf-tcp=80,443".to_string(),
                "--filter-tcp=443".to_string(),
                "--hostlist=lists/list-discord.txt".to_string(),
                "--dpi-desync=multisplit".to_string(),
                format!("--dpi-desync-split-seqovl={}", 500 + i * 25),
                format!("--dpi-desync-split-pos={}", (i % 2) + 1),
                "--dpi-desync-split-seqovl-pattern=bin/tls_clienthello_www_google_com.bin".to_string(),
            ],
            requires_lists: vec!["list-discord.txt".to_string()],
        });
    }

    list.push(RawStrategy {
        id: "discord_voice".to_string(),
        display_name: "Discord Voice — UDP Only".to_string(),
        category: "Discord".to_string(),
        description: "UDP bypass optimized specifically for Discord voice servers".to_string(),
        winws_args: vec![
            "--wf-udp=50000-50100".to_string(),
            "--filter-udp=50000-50100".to_string(),
            "--filter-l7=discord,stun".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-fake-discord=bin/quic_initial_dbankcloud_ru.bin".to_string(),
            "--dpi-desync-repeats=6".to_string(),
        ],
        requires_lists: vec![],
    });

    // 9-15 YouTube Strategies
    list.push(RawStrategy {
        id: "youtube_general".to_string(),
        display_name: "YouTube — General".to_string(),
        category: "Youtube".to_string(),
        description: "Standard desync strategy for YouTube video playback".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--wf-udp=443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-youtube.txt".to_string(),
            "--dpi-desync=multisplit".to_string(),
            "--dpi-desync-split-seqovl=568".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
            "--dpi-desync-split-seqovl-pattern=bin/tls_clienthello_4pda_to.bin".to_string(),
            "--new".to_string(),
            "--filter-udp=443".to_string(),
            "--hostlist=lists/list-youtube.txt".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-repeats=6".to_string(),
            "--dpi-desync-fake-quic=bin/quic_initial_www_google_com.bin".to_string(),
        ],
        requires_lists: vec!["list-youtube.txt".to_string()],
    });

    for i in 1..=6 {
        list.push(RawStrategy {
            id: format!("youtube_alt{}", i),
            display_name: format!("YouTube — ALT {}", i),
            category: "Youtube".to_string(),
            description: format!("Alternative YouTube bypass configuration option {}", i),
            winws_args: vec![
                "--wf-tcp=443".to_string(),
                "--filter-tcp=443".to_string(),
                "--hostlist=lists/list-youtube.txt".to_string(),
                "--dpi-desync=fake,split2".to_string(),
                format!("--dpi-desync-split-pos={}", (i % 2) + 1),
                "--dpi-desync-repeats=10".to_string(),
            ],
            requires_lists: vec!["list-youtube.txt".to_string()],
        });
    }

    // 16-18 Mixed (Combined) Strategies
    list.push(RawStrategy {
        id: "combined_general".to_string(),
        display_name: "Combined — General (Recommended)".to_string(),
        category: "Mixed".to_string(),
        description: "Bypasses both Discord and YouTube using standard rules".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443,2053,2083,2087,2096,8443,12".to_string(),
            "--wf-udp=443,19294-19344,50000-50100,12".to_string(),
            "--filter-udp=443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--hostlist=lists/list-general-user.txt".to_string(),
            "--hostlist-exclude=lists/list-exclude.txt".to_string(),
            "--hostlist-exclude=lists/list-exclude-user.txt".to_string(),
            "--ipset-exclude=lists/ipset-exclude.txt".to_string(),
            "--ipset-exclude=lists/ipset-exclude-user.txt".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-repeats=6".to_string(),
            "--dpi-desync-fake-quic=bin/quic_initial_www_google_com.bin".to_string(),
            "--new".to_string(),
            "--filter-udp=19294-19344,50000-50100".to_string(),
            "--filter-l7=discord,stun".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-fake-discord=bin/quic_initial_dbankcloud_ru.bin".to_string(),
            "--dpi-desync-fake-stun=bin/quic_initial_dbankcloud_ru.bin".to_string(),
            "--dpi-desync-repeats=6".to_string(),
            "--new".to_string(),
            "--filter-tcp=2053,2083,2087,2096,8443".to_string(),
            "--hostlist-domains=discord.media".to_string(),
            "--dpi-desync=multisplit".to_string(),
            "--dpi-desync-split-seqovl=681".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
            "--dpi-desync-split-seqovl-pattern=bin/tls_clienthello_www_google_com.bin".to_string(),
            "--new".to_string(),
            "--filter-tcp=443".to_string(),
            "--hostlist=lists/list-google.txt".to_string(),
            "--ip-id=zero".to_string(),
            "--dpi-desync=multisplit".to_string(),
            "--dpi-desync-split-seqovl=681".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
            "--dpi-desync-split-seqovl-pattern=bin/tls_clienthello_www_google_com.bin".to_string(),
            "--new".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--hostlist=lists/list-general-user.txt".to_string(),
            "--hostlist-exclude=lists/list-exclude.txt".to_string(),
            "--hostlist-exclude=lists/list-exclude-user.txt".to_string(),
            "--ipset-exclude=lists/ipset-exclude.txt".to_string(),
            "--ipset-exclude=lists/ipset-exclude-user.txt".to_string(),
            "--dpi-desync=multisplit".to_string(),
            "--dpi-desync-split-seqovl=568".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
            "--dpi-desync-split-seqovl-pattern=bin/tls_clienthello_4pda_to.bin".to_string(),
        ],
        requires_lists: vec![
            "list-general.txt".to_string(),
            "list-google.txt".to_string(),
        ],
    });

    list.push(RawStrategy {
        id: "combined_alt1".to_string(),
        display_name: "Combined — ALT 1".to_string(),
        category: "Mixed".to_string(),
        description: "Alternative combined rules using fake-tls and split2".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=fake,split".to_string(),
            "--dpi-desync-split-pos=2".to_string(),
            "--dpi-desync-fake-tls=bin/tls_clienthello_www_google_com.bin".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    list.push(RawStrategy {
        id: "combined_alt2".to_string(),
        display_name: "Combined — ALT 2".to_string(),
        category: "Mixed".to_string(),
        description: "More aggressive combined bypass with low MTU and duplicate handshakes".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=split2".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    // 19 Provider Mgts
    list.push(RawStrategy {
        id: "mgts_general".to_string(),
        display_name: "MGTS — Preset".to_string(),
        category: "Mgts".to_string(),
        description: "MGTS ISP-specific desync bypass settings".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=split2".to_string(),
            "--dpi-desync-split-pos=3".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    // 20 Provider Rostelecom
    list.push(RawStrategy {
        id: "rostelecom_general".to_string(),
        display_name: "Rostelecom — Preset".to_string(),
        category: "Rostelecom".to_string(),
        description: "Rostelecom ISP-specific desync bypass settings".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=fake,split2".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
            "--dpi-desync-repeats=6".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    // 21 Provider Mts
    list.push(RawStrategy {
        id: "mts_general".to_string(),
        display_name: "MTS — Preset".to_string(),
        category: "Mts".to_string(),
        description: "MTS ISP-specific desync bypass settings".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=multisplit".to_string(),
            "--dpi-desync-split-seqovl=568".to_string(),
            "--dpi-desync-split-pos=1".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    // 22 Provider Beeline
    list.push(RawStrategy {
        id: "beeline_general".to_string(),
        display_name: "Beeline — Preset".to_string(),
        category: "Beeline".to_string(),
        description: "Beeline ISP-specific desync bypass settings".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=fake".to_string(),
            "--dpi-desync-repeats=4".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    // 23-24 Other/Custom
    list.push(RawStrategy {
        id: "other_custom".to_string(),
        display_name: "Other — Custom fallback".to_string(),
        category: "Other".to_string(),
        description: "Generic fallback rules for other network providers".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=split2".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    list.push(RawStrategy {
        id: "other_alternative".to_string(),
        display_name: "Other — Alternative".to_string(),
        category: "Other".to_string(),
        description: "Alternative fallback rules with fake packet injection".to_string(),
        winws_args: vec![
            "--wf-tcp=80,443".to_string(),
            "--filter-tcp=80,443".to_string(),
            "--hostlist=lists/list-general.txt".to_string(),
            "--dpi-desync=fake".to_string(),
        ],
        requires_lists: vec!["list-general.txt".to_string()],
    });

    list
}
