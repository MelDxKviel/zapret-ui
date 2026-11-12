# zapret2 on Windows

zapret2 is an optional **beta** Windows 10/11 x64 engine. Home and
**Settings → Bypass engine** always show two choices: **zapret1** and
**zapret2 · Beta**. Selecting zapret2 opts into it directly; there is no separate
beta toggle or Auto engine choice. zapret1 remains the default. The Mac utunws
adapter remains the sole Mac engine, without a Windows engine selector.

Home uses one full dashboard. **Start selected** launches the chosen preset;
**Find and start** beside it performs automatic strategy selection. App/core/latest versions, strategy
count, uptime, process mode and PID are visible directly. Service controls,
strategy description, supported filters and installation/update/folder actions
are also shown without a disclosure step. Tuning remains available in Settings.

The dashboard shows installation, scan, running and error states. Installation
shows the selected engine and download progress when available. A scan shows
its engine and current candidate, with Cancel always available. Engine controls
are confined to Home and Settings; the Strategies and Tester pages retain their
own tools without a repeated global engine/error panel.

Automatic strategy selection and detailed testing use only the selected
engine's catalog. Results arrive incrementally on the same Tester page,
including baseline comparisons, reachability and latency. Cancelling automatic
selection keeps the completed result rows available for review. Changing engine
during a scan first cancels it and waits for its tasks and candidate process to
stop before applying the new choice. Manual start and service operations remain
unavailable during the scan.
An engine is stopped before another strategy starts; the successful selected
strategy identifies its engine as well as its preset. Selecting an engine also
removes an owned automatic service for the other engine, so Windows cannot start
that other engine again at the next boot. Reinstall a
service explicitly for the newly selected preset if system startup is desired.

## Official distribution and integrity

The adapter uses [bol-van/zapret2 releases](https://github.com/bol-van/zapret2/releases),
discovered through `releases.atom`, without the GitHub API. The Windows bundle
linked by upstream is [bol-van/zapret-win-bundle](https://github.com/bol-van/zapret-win-bundle).
That repository has no release assets. The zapret2 release ZIP itself contains
the required x64 runtime, so the GUI downloads the official release directly
instead of mixing a moving Windows bundle with unrelated Lua files.

Distribution layout, source and published checksum metadata examined for
release **v1.0.5.2** on 2026-10-08 (see the live-validation limits below):

| Upstream ZIP path | Installed path |
| --- | --- |
| `zapret2-<tag>/binaries/windows-x86_64/winws2.exe` | `bin/winws2.exe` |
| `zapret2-<tag>/binaries/windows-x86_64/cygwin1.dll` | `bin/cygwin1.dll` |
| `zapret2-<tag>/binaries/windows-x86_64/WinDivert.dll` | `bin/WinDivert.dll` |
| `zapret2-<tag>/binaries/windows-x86_64/WinDivert64.sys` | `bin/WinDivert64.sys` |
| `zapret2-<tag>/lua/*` | `lua/*` |
| `zapret2-<tag>/files/fake/*` | `files/fake/*` |

The [official build workflow](https://github.com/bol-van/zapret2/blob/v1.0.5.2/.github/workflows/build.yml)
produces `zapret2-<tag>.zip` and `sha256sum.txt`. The latter covers binaries,
including all four Windows runtime files, but does not cover Lua. Therefore the
installer also requires GitHub's SHA-256 of the *entire release ZIP* and of the
checksum manifest, published in that release's expanded asset list. A missing
digest, checksum mismatch, missing dependency or non-x64 PE image prevents
installation. This verifies the bytes delivered from the official HTTPS release;
it is not a separate maintainer signature. A GitHub asset-page markup change can
require an adapter update; there is no unchecked-download fallback.

Download and extraction have byte/count limits. Unsafe paths, duplicate runtime
paths, symlinks and junctions are rejected. Downloads, extraction and rollback
backups use the sibling `<install-name>-work` directory, normally
`%APPDATA%\zapret-ui\zapret2-work`, on the same volume as the installation.
Installation stages the complete replacement there, preserves user data, then
renames it into place. Failure before promotion leaves the old installation intact; promotion
failure rolls it back. If an external filesystem change prevents rollback, the
old directory is retained for recovery. No upstream `.cmd`, `.bat`, shell script
or Cygwin setup helper is executed.

`version.txt` records the release tag and `release.json` records verified archive
and manifest digests. zapret2 uses its own installation directory, normally
`%APPDATA%\zapret-ui\zapret2`, next to the existing zapret1 directory. In service
mode it is restaged to the separate protected ProgramData copy, with staging
and rollback files in `%ProgramData%\zapret-ui\zapret2-work`. Both loaded Lua
paths and all referenced data paths are absolute, so SCM's working directory
does not affect them. Upstream `nfq2/nfqws.c` invokes `service_run()` automatically
on Windows; no extra `--service` switch is required.

Service replacement validates the protected staging copy before removing the
previous owned registration. The old files, command line, startup policy and
running state are retained for rollback if installation or startup fails. A
rollback that cannot safely stop the rejected service reports the failure and
keeps the backup instead of replacing files beneath a running process. Custom
service accounts or load-order groups are refused before replacement because
their configuration cannot be recreated safely. Stop also joins owned orphan
processes from the protected directory after confirming the service is stopped.

## Presets and custom strategies

The GUI installs three initial native winws2 presets: fake + multisplit,
fake + multidisorder, and fake + fakedsplit. Their TCP HTTP/TLS, QUIC and bounded
Discord UDP profiles use the arguments and packet functions described in the
[zapret2 manual](https://github.com/bol-van/zapret2/blob/v1.0.5.2/docs/manual.en.md)
and the [official Windows example](https://github.com/bol-van/zapret-win-bundle/blob/master/zapret-winws/preset2_example.cmd).
They are candidate configurations, not a promise of compatibility with every ISP.
They use native `--lua-desync`, `--payload` and direction/range controls, never
translate zapret1 `--dpi-desync` flags mechanically. These presets do not need a
TCP timestamp change.

Installed presets are JSON data in `presets/`. To customize one, copy it into
`strategies/` with a filename such as `my-provider.json`, then edit `name`
and `args`. Use ASCII letters, digits, hyphens or underscores for the filename
before the lowercase `.json` extension. Each `args` string becomes exactly one
argument passed directly to `winws2.exe`, including any spaces; do not add shell
quoting. The catalog reloads
these files and prefixes their IDs with `zapret2:user-`. Stop the bypass before
editing, then restart the application to refresh the displayed catalog. The
`category` field is optional and defaults to `Mixed`; a file is limited to
64 KiB and 256 argument strings. Core Lua loaders are added automatically.

```json
{
  "name": "My TLS multisplit",
  "category": "Mixed",
  "args": [
    "--wf-tcp-out=443",
    "--filter-tcp=443",
    "--filter-l7=tls",
    "--hostlist=${ROOT}/lists/list-general-user.txt",
    "--payload=tls_client_hello",
    "--out-range=-d10",
    "--lua-desync=fake:blob=fake_default_tls:tcp_md5",
    "--lua-desync=multisplit:pos=1,midsld"
  ]
}
```

Add the desired domains to `lists/list-general-user.txt`. The presets also use
`lists/list-exclude-user.txt` and `lists/ipset-exclude-user.txt`. Relative list
references must use `${ROOT}/lists/...`; a binary blob can use
`--blob=name:@${ROOT}/files/user/example.bin`. The installer preserves all of
`lists/`, `strategies/` and `files/user/`. Keep customizations in those locations;
`presets/`, upstream Lua and runtime files are refreshed from verified sources.
If a shipped preset was edited directly, the old JSON is retained as a
`strategies/saved-<digest>.json` custom candidate during the update.

The existing zapret1 updater likewise keeps custom `.bat` files and additional
list files. When an old file collides with a different upstream default, the new
default is installed and the original is retained under
`user-backups/presets/<sha256>/` or `user-backups/lists/<sha256>/`. These backups
survive later updates. Copy an old preset back under a new `.bat` name to select
it; explicit `*-user.txt` lists and saved IPSet settings remain in place.

The catalog injects the standard verified `zapret-lib.lua` and
`zapret-antidpi.lua`. It accepts a bounded set of native packet/filter options
and the anti-DPI library's packet functions. Inline Lua, arbitrary Lua files,
`luaexec`, response files, external paths, logging-to-file switches and shell
commands are not supported in custom JSON. This first integration supports
custom combinations/parameters of the shipped packet functions, rather than
running arbitrary downloaded programs with service privileges. Invalid JSON or
unsupported arguments are skipped with a diagnostic in Logs.

## Manual Windows checks

Use a development machine where you control the existing bypass processes and
can authorize the app's normal elevation. Do not run these live checks in CI.

1. Keep an existing `config.toml` with a selected zapret1 strategy. Open the new
   application and check that the strategy and other preferences survive; a legacy Simple preference
   opens the full dashboard. Home and Settings must both show zapret1 and zapret2
   Beta as the only engine choices.
2. In zapret1 mode, start/stop the old preset. Verify runtime status, logs and
   service operations still work.
3. Select zapret2 on Home or in **Settings → Bypass engine**, then install it.
   Installation prepares the selected core. **Find and start** then performs
   automatic strategy selection. Verify `version.txt`, the four `bin/` files,
   Lua files, lists and three catalog entries. Start one preset, check process
   status and connectivity, then Stop. No `winws2.exe` should remain owned by
   the application. A missing/quarantined executable must show a useful error.
4. Copy a preset to `strategies/my-provider.json`, add a domain to the user list,
   and restart the app to refresh the catalog. Update the core and confirm both custom files remain
   unchanged. A deliberately damaged archive/checksum must leave the old core.
5. With both cores installed, select zapret2 and run strategy testing. Results
   must appear incrementally on Tester, contain only zapret2 candidates, compare
   with the stopped baseline, rank connectivity and latency, and leave the
   selected working strategy running. Repeat with zapret1 after stopping the
   bypass. Automatic selection from Home must use the same results page and
   respect the selected engine. Observe that owned `winws.exe` and `winws2.exe`
   never run together.
6. Stop the current bypass before starting a scan, then cancel during baseline,
   candidate probing and winner launch. Confirm the UI leaves its busy state,
   retains the saved selection and completed result rows, and leaves no
   candidate running. A scan requested
   while a bypass is already running is refused without stopping it. Also test a
   manual switch to a missing/rejected executable; a known previous process is
   restored on failure. Change engine during a scan and confirm the old scan is
   fully stopped before the selection changes. Switch back to zapret1 and
   confirm both engine choices remain visible.
7. Test changing from a running zapret1 service to zapret2 and back, including
   service restaging. Check the registered executable and all file arguments
   point to the appropriate protected directory.
8. Start the Telegram proxy, switch/test core strategies, and confirm its port,
   status and Stop behavior remain independent. Quit the app and check child
   processes stop while explicitly installed system services follow existing
   application policy.

## Validation limits

This development environment's Windows antivirus blocked access to the official
v1.0.5.2 ZIP during packaging research. Its checksum manifest, release asset
metadata, official sources and build workflow were inspected without changing
antivirus settings. The application does not perform antivirus scanning,
automatically create Defender exclusions or change Windows protection settings;
this block came from the operating system.
No real winws2 execution, DPI interception or zapret2 SCM
startup smoke test was performed. End-to-end download, extraction and launch
of that real archive therefore remain unverified here.

The implementation includes offline archive/catalog fixtures, configuration
checks and mock lifecycle/tester tests. The final task report records the actual
test and Windows build results; these tests do not establish ISP bypass
effectiveness. No macOS build or native macOS validation was performed as part
of this Windows-only integration. The existing Mac utunws adapter remains the
Mac core; zapret2 is not available on that platform.

For an independently source-verified local Windows runtime, an optional
`--dry-run` **as the first argument**, followed by the preset's resolved argv,
checks argument parsing and required data-file access without entering the
WinDivert startup function. It does not execute the Lua packet functions or
prove that a strategy bypasses DPI. `--intercept=0` is unsuitable for a
driver-free smoke check: the Windows implementation still initializes
WinDivert before running Lua and exiting. A different local bundle must have
its own verified provenance; a matching filename is insufficient.

## Future blockcheck2 integration

[blockcheck2](https://github.com/bol-van/zapret2/blob/v1.0.5.2/blockcheck2.sh) is a
separate strategy exploration tool. The official Windows bundle launches it
through Cygwin and supplies curl and supporting executables. Its own instructions
require other bypass engines to be stopped. It varies packet strategies and
network probes rather than just ranking the GUI's preset catalog.

A later opt-in adapter could use an independently verified matching Windows
bundle, exclusive ownership of the interceptor, a bounded subprocess/job tree,
explicit probe targets, cancellation that joins every child, and conversion of
successful native arguments into reviewed JSON presets. Its interactive shell
configuration/output is not a stable structured API; importing output directly
as executable shell or Lua would be inappropriate. None of these generators or
Cygwin shell tools are installed or started by this initial integration.
