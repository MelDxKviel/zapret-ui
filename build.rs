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
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set_manifest_file("assets/app.manifest");
        res.compile().unwrap();
    }
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
