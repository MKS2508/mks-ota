# Third-party attribution

## `tauri-plugin-updater` 2.10.1

- Source: <https://github.com/tauri-apps/plugins-workspace/tree/v2/plugins/updater>
- License: `Apache-2.0 OR MIT`
- Copyright: Tauri Programme within The Commons Conservancy

`src/install/full/macos.rs` in this crate derives its extract-and-swap logic from
`plugins/updater/src/updater.rs` (commit tagged `v2`, lines 1288-1381 — the macOS branch of
`install_inner`): extracting the downloaded `.tar.gz` to a temp directory while skipping the
archive's leading path component, then a two-step `rename` (current bundle to a backup, new
bundle into place) with an AppleScript privilege-escalation fallback when the first `rename`
fails with `PermissionDenied`.

What was **not** carried over from `updater.rs`:

- the `RemoteRelease` manifest shape and its `{{target}}`/`{{arch}}`/`{{current_version}}` URL
  templating — this crate deserializes the hub's own JSON shape instead (`src/manifest.rs`),
  per ADR-0045 D8-B ("el hub NO se moldea al cliente");
- buffering the artifact entirely in memory before verifying (`updater.rs:730-742`) — this
  crate streams to a temp file and verifies with constant RAM (`src/download.rs`,
  `src/verify.rs`);
- the Windows NSIS/MSI install path — implemented in this crate as `install/full/linux.rs`
  (see ADR-0046). The AppImage path was derived from `updater.rs:101-113` (`APPIMAGE`
  env-var lookup) and `updater.rs:1064` (same-device `rename` check, `TempDirNotOnSameMountPoint`);
  the deb path uses `dpkg -i` as a fallback for non-AppImage installs.

What was **added** on top of the derived logic, because the original either lacks it or has a
latent bug here:

- a same-device (`dev()`) check before the final `rename`, which upstream only performs on
  Linux (`updater.rs:1064`, `TempDirNotOnSameMountPoint`) — its absence on macOS is a bug this
  crate does not inherit;
- relaunching the app after install — upstream's own doc comment says "you need to relaunch the
  app" on macOS/Linux (`updater.rs:747-750`) and does not do it.

## `minisign-verify` 0.2.5

- Source: <https://github.com/jedisct1/rust-minisign-verify>
- License: MIT
- Author: Frank Denis (jedisct1)

Not modified — used as a dependency (`src/verify.rs`), not vendored.
