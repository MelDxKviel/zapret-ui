use crate::contracts::{Category, EngineKind, Strategy};
use crate::ports::StrategyCatalog;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_PRESET_BYTES: u64 = 64 * 1024;
const MAX_PRESETS: usize = 256;

/// Each string is one argv element. Neither a shell nor a command-file parser is
/// involved. Core Lua libraries are supplied by the verified distribution only.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Preset {
    name: String,
    #[serde(default = "mixed")]
    category: Category,
    args: Vec<String>,
}

fn mixed() -> Category {
    Category::Mixed
}

pub struct Zapret2Catalog {
    install_dir: PathBuf,
}

impl Zapret2Catalog {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }

    fn scan(&self) -> Vec<Strategy> {
        if !super::is_valid_install_dir(&self.install_dir) {
            return Vec::new();
        }
        let mut files = Vec::new();
        for folder in ["presets", "strategies"] {
            let dir = self.install_dir.join(folder);
            if !std::fs::symlink_metadata(&dir)
                .is_ok_and(|meta| meta.is_dir() && !super::is_link(&meta))
            {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten().take(MAX_PRESETS) {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "json") {
                        files.push((folder, path));
                    }
                }
            }
        }
        files.sort_by(|a, b| a.1.cmp(&b.1));
        files
            .into_iter()
            .filter_map(|(folder, path)| match self.read_preset(folder, &path) {
                Ok(strategy) => Some(strategy),
                Err(error) => {
                    tracing::warn!(
                        "Ignoring invalid zapret2 preset {}: {error:#}",
                        path.display()
                    );
                    None
                }
            })
            .collect()
    }

    fn read_preset(&self, folder: &str, path: &Path) -> Result<Strategy> {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_file() || super::is_link(&meta) || meta.len() > MAX_PRESET_BYTES {
            bail!("Preset must be a regular JSON file of at most 64 KiB");
        }
        let preset: Preset = serde_json::from_slice(&std::fs::read(path)?)
            .context("Invalid zapret2 strategy JSON")?;
        if preset.name.trim().is_empty()
            || preset.name.len() > 128
            || preset.name.chars().any(char::is_control)
        {
            bail!("Invalid strategy name");
        }
        if preset.args.is_empty() || preset.args.len() > 256 {
            bail!("Expected 1..256 argv elements");
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("Invalid preset filename")?;
        if stem.is_empty()
            || !stem
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            bail!("Preset filename must use ASCII letters, digits, hyphens or underscores");
        }
        if folder == "presets" && stem.starts_with("user-") {
            bail!("The user- prefix is reserved for custom strategy IDs");
        }
        let mut args = ["zapret-lib.lua", "zapret-antidpi.lua"]
            .into_iter()
            .map(|name| {
                format!(
                    "--lua-init=@{}",
                    self.install_dir.join("lua").join(name).display()
                )
            })
            .collect::<Vec<_>>();
        let mut requires_lists = Vec::new();
        for arg in preset.args {
            args.push(resolve_argument(
                &self.install_dir,
                &arg,
                &mut requires_lists,
            )?);
        }
        // Built-in and user IDs cannot collide even when the same file is copied.
        let prefix = if folder == "strategies" { "user-" } else { "" };
        Ok(Strategy {
            id: format!("zapret2:{prefix}{stem}"),
            display_name: preset.name,
            engine: EngineKind::Zapret2,
            category: preset.category,
            description: String::new(),
            winws_args: args,
            requires_lists,
        })
    }
}

impl StrategyCatalog for Zapret2Catalog {
    fn all(&self) -> Vec<Strategy> {
        self.scan()
    }
    fn by_id(&self, id: &str) -> Option<Strategy> {
        self.scan().into_iter().find(|s| s.id == id)
    }
    fn by_category(&self, category: Category) -> Vec<Strategy> {
        self.scan()
            .into_iter()
            .filter(|s| s.category == category)
            .collect()
    }
}

fn resolve_argument(root: &Path, arg: &str, lists: &mut Vec<String>) -> Result<String> {
    if arg.len() > 4096 || arg.chars().any(char::is_control) {
        bail!("Invalid strategy argument length or control character");
    }
    if arg == "--new" {
        return Ok(arg.into());
    }
    let (key, value) = arg
        .split_once('=')
        .context("Options must use --name=value")?;
    if value.is_empty() {
        bail!("Empty strategy option");
    }
    match key {
        "--hostlist" | "--hostlist-exclude" | "--ipset" | "--ipset-exclude" => {
            let relative = value
                .strip_prefix("${ROOT}/")
                .context("List paths must start with ${ROOT}/")?;
            if !relative.starts_with("lists/") {
                bail!("List paths must be inside lists/");
            }
            let path = resolve_data_path(root, relative)?;
            lists.push(path.to_string_lossy().into_owned());
            return Ok(format!("{key}={}", path.display()));
        }
        "--blob" => {
            let (name, data) = value.split_once(':').context("Expected blob name:data")?;
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                bail!("Invalid blob name");
            }
            if let Some(relative) = data.strip_prefix("@${ROOT}/") {
                if !relative.starts_with("files/") {
                    bail!("Blob paths must be inside files/");
                }
                return Ok(format!(
                    "{key}={name}:@{}",
                    resolve_data_path(root, relative)?.display()
                ));
            }
            if !data.strip_prefix("0x").is_some_and(|hex| {
                !hex.is_empty() && hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit())
            }) {
                bail!("Blob must be hexadecimal or @${{ROOT}}/files/path");
            }
        }
        "--lua-desync" => {
            let function = value.split(':').next().unwrap_or("");
            // No luaexec/condition expressions or arbitrary Lua. These names are
            // exported packet operations from upstream zapret-antidpi.lua.
            if !matches!(
                function,
                "fake"
                    | "multisplit"
                    | "multidisorder"
                    | "multidisorder_legacy"
                    | "fakedsplit"
                    | "fakeddisorder"
                    | "hostfakesplit"
                    | "syndata"
                    | "synack"
                    | "synack_split"
                    | "rst"
                    | "wsize"
                    | "wssize"
                    | "udplen"
                    | "dht_dn"
                    | "http_domcase"
                    | "http_hostcase"
                    | "http_methodeol"
                    | "http_unixeol"
                    | "tcpseg"
                    | "oob"
                    | "pktmod"
                    | "drop"
                    | "send"
            ) {
                bail!("Unsupported Lua packet operation");
            }
            for parameter in value.split(':').skip(1) {
                let key = parameter.split('=').next().unwrap_or("");
                if !matches!(
                    key,
                    "blob"
                        | "pattern"
                        | "seqovl"
                        | "seqovl_pattern"
                        | "pos"
                        | "tcp_md5"
                        | "tcp_seq"
                        | "tcp_ack"
                        | "tcp_ts"
                        | "tcp_ts_up"
                        | "ip_ttl"
                        | "ip6_ttl"
                        | "ip_autottl"
                        | "ip6_autottl"
                        | "ip_id"
                        | "ip6_flowlabel"
                        | "badsum"
                        | "repeats"
                        | "tls_mod"
                        | "nofake1"
                        | "nofake2"
                        | "nofake3"
                        | "nofake4"
                        | "nodrop"
                        | "optional"
                        | "direction"
                        | "payload"
                        | "wsize"
                        | "scale"
                        | "len"
                        | "increment"
                ) || !parameter
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.,+-<>=!".contains(&b))
                {
                    bail!("Unsupported Lua packet parameter; only scalar packet data is allowed");
                }
            }
        }
        "--wf-tcp-out" | "--wf-tcp-in" | "--wf-udp-out" | "--wf-udp-in" | "--wf-l3"
        | "--wf-iface" | "--wf-tcp-empty" | "--wf-filter-lan" | "--filter-tcp" | "--filter-udp"
        | "--filter-l7" | "--filter-l3" | "--payload" | "--in-range" | "--out-range"
        | "--filter-ssid" | "--filter-nlm" => {
            if value.contains(['@', '$', ';', '\'', '"', '\\']) {
                bail!("Unexpected file or code in strategy option");
            }
        }
        _ => bail!("Unsupported zapret2 option: {key}"),
    }
    Ok(arg.into())
}

fn resolve_data_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if !super::safe_relative(relative) {
        bail!("Unsafe relative data path");
    }
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
        let meta =
            std::fs::symlink_metadata(&path).context("Referenced strategy data is missing")?;
        if super::is_link(&meta) {
            bail!("Strategy data must not use links or junctions");
        }
    }
    if !path.is_file() {
        bail!("Strategy data must be a file");
    }
    Ok(path)
}

/// Materialize reviewed native winws2 argv presets. They are inputs to the
/// catalog, editable by copying into strategies/, never executable scripts.
pub(crate) fn write_defaults(root: &Path) -> Result<()> {
    for dir in ["presets", "strategies", "lists", "files/user"] {
        std::fs::create_dir_all(root.join(dir))?;
    }
    std::fs::write(root.join("lists/list-general.txt"),
        "youtube.com\nyoutu.be\ngooglevideo.com\nytimg.com\nyoutubei.googleapis.com\ndiscord.com\ndiscord.gg\ndiscordapp.com\ndiscordapp.net\ndiscord.media\ngoogle.com\ncloudflare.com\n")?;
    for name in [
        "list-general-user.txt",
        "list-exclude-user.txt",
        "ipset-exclude-user.txt",
    ] {
        std::fs::write(root.join("lists").join(name), "")?;
    }
    for (id, name, technique) in [
        (
            "multisplit",
            "TLS / HTTP · multisplit",
            "multisplit:pos=1,midsld",
        ),
        (
            "multidisorder",
            "TLS / HTTP · multidisorder",
            "multidisorder:pos=1,midsld",
        ),
        (
            "fakedsplit",
            "TLS / HTTP · fakedsplit",
            "fakedsplit:pos=midsld:tcp_md5",
        ),
    ] {
        let mut args = vec!["--wf-tcp-out=80,443", "--wf-udp-out=443,50000-50100"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        for (filter, payload, fake) in [
            (
                "--filter-tcp=80",
                "http_req",
                "fake:blob=fake_default_http:tcp_md5",
            ),
            (
                "--filter-tcp=443",
                "tls_client_hello",
                "fake:blob=fake_default_tls:tcp_md5:repeats=6:tls_mod=rnd,dupsid",
            ),
        ] {
            args.extend(
                [
                    filter,
                    "--hostlist=${ROOT}/lists/list-general.txt",
                    "--hostlist=${ROOT}/lists/list-general-user.txt",
                    "--hostlist-exclude=${ROOT}/lists/list-exclude-user.txt",
                    "--ipset-exclude=${ROOT}/lists/ipset-exclude-user.txt",
                    "--out-range=-d10",
                ]
                .into_iter()
                .map(String::from),
            );
            args.extend([
                format!("--payload={payload}"),
                format!("--lua-desync={fake}"),
                format!("--lua-desync={technique}"),
                "--new".into(),
            ]);
        }
        args.extend(
            [
                "--filter-udp=443",
                "--filter-l7=quic",
                "--payload=quic_initial",
                "--hostlist=${ROOT}/lists/list-general.txt",
                "--hostlist=${ROOT}/lists/list-general-user.txt",
                "--hostlist-exclude=${ROOT}/lists/list-exclude-user.txt",
                "--ipset-exclude=${ROOT}/lists/ipset-exclude-user.txt",
                "--lua-desync=fake:blob=fake_default_quic:repeats=11",
                "--new",
                "--filter-udp=50000-50100",
                "--filter-l7=stun,discord",
                "--payload=stun,discord_ip_discovery",
                "--lua-desync=fake:blob=0x00000000000000000000000000000000:repeats=2",
            ]
            .into_iter()
            .map(String::from),
        );
        let preset = Preset {
            name: name.into(),
            category: Category::Mixed,
            args,
        };
        std::fs::write(
            root.join("presets").join(format!("{id}.json")),
            serde_json::to_vec_pretty(&preset)?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in super::super::RUNTIME_FILES {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "fixture").unwrap();
        }
        write_defaults(dir.path()).unwrap();
        dir
    }

    #[test]
    fn discovers_native_presets_and_keeps_custom_ids_separate() {
        let dir = fixture();
        std::fs::copy(
            dir.path().join("presets/multisplit.json"),
            dir.path().join("strategies/multisplit.json"),
        )
        .unwrap();
        let all = Zapret2Catalog::new(dir.path().into()).all();
        assert_eq!(all.len(), 4);
        assert!(all
            .iter()
            .all(|s| s.engine == EngineKind::Zapret2 && s.id.starts_with("zapret2:")));
        assert!(all.iter().any(|s| s.id == "zapret2:user-multisplit"));
        assert!(all.iter().all(|s| s
            .winws_args
            .iter()
            .all(|a| !a.contains("${ROOT}") && !a.contains("--dpi-desync"))));
        assert!(all
            .iter()
            .all(|s| s.winws_args[0].contains("zapret-lib.lua")));
    }

    #[test]
    fn absent_binary_or_lua_yields_no_catalog() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("lua/zapret-lib.lua")).unwrap();
        assert!(Zapret2Catalog::new(dir.path().into()).all().is_empty());
    }

    #[test]
    fn rejects_code_shell_options_and_external_or_traversal_paths() {
        let dir = fixture();
        for arg in [
            "--lua-init=os.execute('calc')",
            "--lua-desync=luaexec:code=evil()",
            "--lua-desync=fake:condition=os.execute[[calc]]",
            "--lua-desync=fake:blob=os.execute[[calc]]",
            "--dpi-desync=fake",
            "--hostlist=C:/private.txt",
            "--hostlist=${ROOT}/lists/../../private.txt",
            "--hostlist=${ROOT}/lists/NUL",
            "--blob=x:@C:/private.txt",
            "--debug=@private.txt",
            "@args.txt",
            "cmd /c test",
        ] {
            assert!(
                resolve_argument(dir.path(), arg, &mut Vec::new()).is_err(),
                "{arg}"
            );
        }
    }

    #[test]
    fn paths_with_spaces_are_single_argv_elements_and_rebase_for_service() {
        let dir = fixture();
        let root = dir.path().join("protected core");
        std::fs::create_dir_all(root.join("lists")).unwrap();
        std::fs::write(root.join("lists/custom list.txt"), "example.com").unwrap();
        let mut lists = Vec::new();
        let arg = resolve_argument(
            &root,
            "--hostlist=${ROOT}/lists/custom list.txt",
            &mut lists,
        )
        .unwrap();
        assert_eq!(
            arg,
            format!(
                "--hostlist={}",
                root.join("lists").join("custom list.txt").display()
            )
        );
        assert_eq!(lists.len(), 1);
    }
}
