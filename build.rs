fn main() {
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // Display version, resolved at build time so releases and the About page
    // always agree without hand-editing Cargo.toml:
    //   1. ZAPRET_UI_VERSION  — set by CI from the pushed git tag (source of truth)
    //   2. `git describe`     — e.g. "v0.1.0-5-gabc123" for local/dev builds
    //   3. CARGO_PKG_VERSION  — fallback when no git history (source tarballs)
    println!("cargo:rerun-if-env-changed=ZAPRET_UI_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    let raw = std::env::var("ZAPRET_UI_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let version = format!("v{}", raw.trim().trim_start_matches('v'));
    println!("cargo:rustc-env=APP_VERSION={version}");

    slint_build::compile("ui/main_window.slint").unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        embed_windows_resources();
    }
}

/// Embed the icon and application manifest into the exe.
///
/// Release builds ship to end users and must always launch elevated — the app
/// installs/controls a Windows service and the WinDivert driver, both of which
/// need admin — so the release manifest requests `requireAdministrator` (a UAC
/// prompt on launch). Dev builds (`cargo run`, `cargo run --example ui_only`,
/// tests) keep `asInvoker` so the mock-backed UI iteration workflow doesn't fire
/// a UAC prompt on every launch.
fn embed_windows_resources() {
    let require_admin = std::env::var("PROFILE").as_deref() == Ok("release");

    // Single source of truth is `assets/app.manifest` (asInvoker); for release we
    // swap only the requested execution level and emit the result to OUT_DIR, so
    // the two variants can never drift apart.
    let manifest_src =
        std::fs::read_to_string("assets/app.manifest").expect("read assets/app.manifest");
    let manifest = if require_admin {
        manifest_src.replace(r#"level="asInvoker""#, r#"level="requireAdministrator""#)
    } else {
        manifest_src
    };
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_path = std::path::Path::new(&out_dir).join("app.manifest");
    std::fs::write(&manifest_path, manifest).expect("write generated manifest");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set_manifest_file(manifest_path.to_str().expect("manifest path is valid UTF-8"));
    res.compile().expect("compile Windows resources");
}

/// `git describe --tags --always --dirty`, or `None` if git is unavailable
/// (e.g. building from a source tarball with no `.git`).
fn git_describe() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

