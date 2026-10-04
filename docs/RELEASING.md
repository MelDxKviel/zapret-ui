# Releasing

Versioning is **tag-driven** — the git tag is the single source of truth, so
there's no Cargo.toml version to bump by hand.

## Cut a release

```powershell
git tag v1.2.3          # SemVer, with a leading "v"
git push origin v1.2.3
```

That's it. The `CI / Release` workflow then:

1. Runs Windows and native Apple Silicon checks (clippy + offline tests),
   including the reusable `macos.yml` workflow. Mac checks never install the
   bypass core or change network/service configuration.
2. Builds each release binary with `ZAPRET_UI_VERSION=1.2.3` in the environment
   (this build runs **only on tags**, not on regular commits).
   `build.rs` stamps that into `APP_VERSION`, so the shipped `.exe` and the
   **About** page report exactly `v1.2.3`. The Mac bundle uses the same version;
   its numeric Apple version fields omit any prerelease suffix, which remains
   visible in About and the `ZapretUIVersion` bundle metadata.
3. Publishes a GitHub Release with auto-generated notes, attaching
   the Windows EXE, the macOS ARM64 ZIP and DMG, and a SHA-256 file for each.
   Publishing waits for both platforms to pass. The existing EXE asset names
   remain unchanged for Windows self-update.

## macOS packaging and signing

`sh scripts/build-macos.sh` produces the `.app`, ZIP, DMG and checksums in
`dist/`. The DMG contains a shortcut to Applications. Only the GUI is bundled;
the Flowseal core is downloaded at runtime.

Local and CI builds use an ad-hoc signature by default. This verifies bundle
integrity but does not bypass Gatekeeper on another Mac. Users may need
**Privacy & Security → Open Anyway**. Do not describe these builds as notarized.

For a maintainer build, `ZAPRET_UI_SIGNING_IDENTITY` can name an existing
Developer ID Application identity in the login keychain. The script then uses
the hardened runtime and a secure timestamp. Apple notarization and stapling
must be completed separately before publishing a notarized distribution; the
CI workflow does not install signing credentials or claim notarization.

To repackage an existing native release binary, pass `--package-only`, keeping
the same checkout and `ZAPRET_UI_VERSION` used to build it. Normal builds are
preferred because they keep the executable and bundle version in sync.

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
