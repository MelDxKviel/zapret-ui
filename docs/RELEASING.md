# Releasing

Versioning is **tag-driven** — the git tag is the single source of truth, so
there's no Cargo.toml version to bump by hand.

## Cut a release

```powershell
git tag v1.2.3          # SemVer, with a leading "v"
git push origin v1.2.3
```

That's it. The `CI / Release` workflow then:

1. Runs the `Lint & Test` checks (clippy + tests) — same as on every push/PR.
2. Builds the release binary with `ZAPRET_UI_VERSION=1.2.3` in the environment
   (this build runs **only on tags**, not on regular commits).
   `build.rs` stamps that into `APP_VERSION`, so the shipped `.exe` and the
   **About** page report exactly `v1.2.3`.
3. Publishes a GitHub Release with auto-generated notes, attaching
   `zapret-ui.exe` and `zapret-ui.exe.sha256`.

## Pre-releases

Tags containing a hyphen are flagged as pre-releases automatically:

```powershell
git tag v1.3.0-rc.1
git push origin v1.3.0-rc.1
```

## How the version is resolved (build.rs)

In priority order:

1. **`ZAPRET_UI_VERSION`** — set by CI from the tag (release builds).
2. **`git describe --tags --always --dirty`** — local/dev builds, e.g.
   `v1.2.3-5-gabc123` or `v1.2.3-dirty`.
3. **`CARGO_PKG_VERSION`** — fallback when there's no git history.

So local builds show their commit distance from the last tag, and released
builds show a clean tag — no manual editing, no drift.
