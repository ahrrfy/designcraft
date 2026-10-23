# Windows installer UI

Native development previews can be launched with `start-native-preview.ps1 -ManifestPath <absolute-json-path>`.
The launcher reads and writes UTF-8 explicitly, including on Windows PowerShell 5, and preserves
the existing profile used to scope protected provider keys. Missing profile/document/executable
paths stop startup instead of creating an unrelated blank session. `-ValidateOnly` checks the
paths and recorded executable hash without starting the app or contacting providers; automated
probes can use `-Hidden`. Run `powershell.exe -NoProfile -File packaging/windows/test-preview-launcher.ps1`
to verify Arabic paths, UTF-8 with and without a BOM, read-only validation and missing-path rejection.

`update-native-preview.ps1 -ManifestPath <absolute-json-path>` checks published
`v*-ai*` previews only in `ahrrfy/designcraft`. Release packaging (triggered automatically
after successful `Rust CI` on `main`, and automatically every 6 hours by `upstream-sync.yml`
when upstream `storytold/designcraft` commits merge cleanly and pass full Windows quality gates)
supplies `update.json` with archive and executable SHA-256, size, source commit, optional
`syncBaseCommit`, and a verified CI URL. The updater verifies that proof and forward-only
ancestry, bounds archive expansion, rejects traversal/scripts, installs side by side and
retains the old version. These previews are unsigned; hashes and GitHub CI prove source/channel
integrity, not a code-signing identity. Upstream releases are integrated and verified in the
fork before becoming eligible.

A running app is never closed: the updater atomically records its verified staged
pointer while retaining the active executable. The next launcher invocation, after
the app has closed, verifies both EXE and CLI SHA-256 and `SOURCE-COMMIT.txt` again,
then atomically promotes the pointer and retains the previous executable. A corrupt
or missing stage stops launch without modifying the manifest, profile or document.
`-ValidateOnly` also validates pending stages but never promotes or writes anything.
The launcher exports `DESIGNCRAFT_PREVIEW_MANIFEST` and `DESIGNCRAFT_CONFIG_DIR`, and
records its absolute trusted sibling `update_script_path` for the update UI.
Existing profile, provider
vault scope and document paths remain unchanged. No generated pages, approval or
print action is resumed. Missing metadata, unsuccessful CI, an unknown installed
commit, network failure or a divergent candidate stops installation. A published
ancestor of the installed development build reports `up_to_date` with the current
commit and executable; an older published release is never installed.
Published drafts are not eligible. `test-preview-updater.ps1` exercises the
archive/source/digest guards and preservation with synthetic local files and no
provider or network requests.

For automatic background update checks on Windows, dot-source `start-native-preview.ps1 -ManifestPath <absolute-json-path> -LibraryOnly`
and invoke `Install-PreviewUpdateScheduledTask <absolute-manifest> 'Smart Design Auto Update' 120`.
This registers a hidden per-user scheduled task (network-only, battery-safe) that stages or
promotes verified updates for the next launch without closing an open session.
Source development and publication remain separate from user installation.

Every successful updater invocation prints exactly one compressed JSON object:
`{"status":"up_to_date","selectedCommit":"<40-character-sha>","targetExe":"<absolute-path>","ciUrl":null}`.
Other statuses are `no_published_update` (all three targets null),
`staged_waiting_for_app_close`, and `installed_for_next_launch`. CI URLs, when
present, refer to the trusted fork's verified GitHub Actions run. Errors produce
one concise stderr message and exit nonzero. No Git checkout, build, installed
ArtCraft Launcher, provider request or open application is modified.

For one-time shortcut setup, dot-source the launcher with `-LibraryOnly` and invoke
`Install-PreviewLauncherShortcuts <absolute-manifest> <absolute-shortcut-directory>`.
It creates `Smart Design Launcher.lnk` (open/promote) and
`Check Smart Design Updates.lnk` (check/stage), each invoking the local script via
hidden Windows PowerShell. The existing ArtCraft shortcuts are preserved. The
in-app update button supplies visible asynchronous status for check/stage results.
`Promote-StagedUpdate`, `Resolve-PreviewLaunch` and `Get-PreviewUpdateTaskDefinition`
are library helpers: tests invoke them without starting a binary. Both launcher and
updater serialize normal actions with the same exclusive update lock.

Run `powershell.exe -NoProfile -File packaging/windows/test-preview-update-flow.ps1`
for synthetic archive and CI/upstream-sync guards, persisted staging, all corrupt/missing stage
files, next-launch promotion, read-only validation, scheduled-task spec validation, nested settings and environment
preservation, shortcut creation/inspection and every JSON success outcome. Its
network/process doubles make no real requests and never execute fixture binaries.

`installer-ui.wxs` is generated from the shared `craft-installer` crate in
[craft-libs](https://github.com/storytold/craft-libs/tree/main/crates/installer).
`package.ps1` compiles it with this app's package source. The common UI provides progress,
repair/remove, and explicit install/uninstall/repair success, cancellation and failure screens.

Regenerate or check from a sibling craft-libs checkout:

```sh
cargo run --locked -p craft-installer -- --write --app designcraft ../designcraft/packaging/windows/installer-ui.wxs
cargo run --locked -p craft-installer -- --check --app designcraft ../designcraft/packaging/windows/installer-ui.wxs
```

Edit the shared templates/profile and regenerate; avoid editing this snapshot by hand. The
profile retains this app's existing folder, shortcut and launch options; defaults, persistence,
components, associations and signing stay in the app-owned source. Release builds require no
sibling checkout, published crate or additional runtime dependency. `/qn`, `/qb` and `/passive`
remain unattended and do not launch the app.

Source/profile checks and native fixture MSI linking/UI table checks passed for x64, x86 and
ARM64. All 14 profiles passed full UI install, missing-file repair and cached uninstall, and
x86 silent install/removal. Folder upgrades, desktop choices and all six launch profiles also
passed their Windows fixture checks. See the
[Windows verification record](../../../craft-libs/docs/windows-installer-verification.md) for
interactive results, scope validation and remaining acceptance checks with real release payloads.
The shared crate includes reusable linking, full UI, option/upgrade/launch and x86 test runners.
Windows Settings now uses a separate MSI-owned entry whose uninstall command selects full UI,
including the completion screen. Its quiet command and direct `/qn`, `/qb`, `/passive` commands
remain unattended. The main feature references `CraftInstallerSettings`; the shared snapshot
owns this registration and hides the native MSI entry while retaining MSI repair/upgrade APIs.

The Settings display name appends `(Craft Apps)` for compatibility with older Launchers; the
hidden native MSI name stays unchanged. Existing ARP Modify policy is retained explicitly.
