# Releasing Windows and macOS together

There is one repository, one application version and one release for both
platforms. The git tag is the version source; do not bump `Cargo.toml` just to
cut a release or maintain a separate macOS release branch. The application
downloads its platform's Flowseal core at runtime; neither core is bundled.

## Checks before a release

The single [`CI / Release`](../.github/workflows/release.yml) workflow runs on
pushes to `main`, `master` and `codex/**`, PRs targeting `main`/`master`, `v*`
tags, and manual dispatch. Its shared matrix checks the same revision on:

| Platform | Native runner | Rust target | Distribution |
| --- | --- | --- | --- |
| Windows 10/11 x64 | `windows-2022` | `x86_64-pc-windows-msvc` | Single EXE |
| macOS 14+ Apple Silicon | `macos-15` (ARM64) | `aarch64-apple-darwin` | App bundle in ZIP and DMG |

Both jobs install the pinned toolchain from `rust-toolchain.toml`, check
formatting, run Clippy with warnings denied, run all default tests, and build
the release binary once. Dependencies are fetched with `--locked`; compilation
and tests use `--frozen` so the lockfile cannot change and Cargo cannot access
the network during those steps. Rust test execution itself stays offline,
apart from local loopback fixtures. The workflow never downloads or runs the
bypass core, changes PF/TCP/hosts settings, or installs a system service.
Do not set `ZAPRET_UI_RUN_SERVICE_TESTS` or pass `--ignored` in CI: live SCM and
Telegram connectivity tests require explicit local opt-in.

Each successful platform job uploads its distribution as an Actions artifact,
including for PRs and manual runs. Use both native job results for review;
passing macOS tests locally does not establish Windows compatibility. A failing
platform does not cancel the other platform's diagnostics.

## Cut a release

After the shared branch passes both platforms, create and push a new SemVer tag:

```sh
git tag -a v1.2.3 -m "zapret-ui v1.2.3"
git push origin v1.2.3
```

The workflow resolves `ZAPRET_UI_VERSION=1.2.3` once and supplies it to both
native builds. The executable and About page report `v1.2.3`; the macOS bundle
records the same version in `ZapretUIVersion`. Apple's numeric bundle version
fields omit prerelease/build suffixes, while About retains them.

Only a tag **push** publishes. Manual dispatch always stops after checks and
packaging, even when a tag is selected. Publication requires both platform jobs
to pass and reuses their exact artifacts, without recompiling Windows:

1. Download the Windows and macOS artifacts from that same workflow run.
2. Require exactly these six nonempty files and verify each package's SHA-256:

   ```text
   zapret-ui.exe
   zapret-ui.exe.sha256
   zapret-ui-macos-arm64.zip
   zapret-ui-macos-arm64.zip.sha256
   zapret-ui-macos-arm64.dmg
   zapret-ui-macos-arm64.dmg.sha256
   ```

3. Create a draft release with generated notes, or resume an existing draft.
4. Upload all six assets, download the draft's assets again, and verify both
   their checksums and byte-for-byte hashes against the checked CI outputs.
5. Publish the complete release. Users and the updater never see the partial
   draft. The established Windows asset names stay compatible with self-update.

If upload or validation fails, the draft remains unpublished. Fix the reported
problem and rerun the failed jobs for the same tag; draft uploads are replaced
on retry. Unexpected assets also block publication and must be reviewed before
retrying. Do not manually publish a partial draft. Published releases are never
overwritten or converted back to drafts by the workflow; ship a new version
for a correction. If the final publish request loses its connection, check the
release status first: GitHub may already have published the complete release.
Tags are not canceled by newer CI runs.

To validate downloaded release assets locally in a directory containing only
those six files:

```sh
python3 scripts/release_assets.py /path/to/downloads
python3 -m unittest discover -s scripts -p 'test_release_assets.py' -v
```

## macOS packaging and signing

`sh scripts/build-macos.sh` produces `dist/Zapret UI.app`, ZIP, DMG and
checksums. The DMG contains a shortcut to Applications. CI first builds the
native ARM64 binary and calls the same script with `--package-only`, keeping
the checkout and `ZAPRET_UI_VERSION` identical for compilation and packaging.

Local and CI builds use an ad-hoc signature by default. The packager verifies
the signature and disk image, but ad-hoc signing does not establish a trusted
Developer ID on another Mac. Users may need **Privacy & Security → Open
Anyway**. Do not describe these builds as notarized.

For a maintainer build, `ZAPRET_UI_SIGNING_IDENTITY` can name an existing
Developer ID Application identity in the login keychain. The script then uses
the hardened runtime and a secure timestamp. Apple notarization and stapling
must be completed separately before publishing a notarized distribution; CI
does not install signing credentials or claim notarization.

## Prereleases and local versions

Tags with a SemVer prerelease component, such as `v1.3.0-rc.1`, are published
as prereleases for **both** platforms. A `+build` suffix alone is not a
prerelease. Invalid version tags fail before native builds start.

`build.rs` resolves the display version from `ZAPRET_UI_VERSION`, then
`git describe --tags --always --dirty`, then `CARGO_PKG_VERSION` if Git metadata
is unavailable. Local builds therefore show their revision; tagged releases
show the same clean version on Windows and macOS.
