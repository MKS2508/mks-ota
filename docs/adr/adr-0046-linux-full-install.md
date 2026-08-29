# ADR-0046: Linux full-install — AppImage primary, deb fallback

## Status

proposed

## Context

TPV (`tpv-el-haido2`) ships for both macOS and Linux. After killing the
`tauri-plugin-updater` OTA channel (commit `2523408`) because its hub endpoint
returns 404 since M6, the full-update path is only implemented for macOS via
`install::full::macos`. Linux has no full-update mechanism.

`wraith-linux` and TPV Linux both distribute as **AppImage** bundles. Tauri
apps running from an AppImage get their own path via the `$APPIMAGE` env var
set by the AppImage runtime — not from `/proc/self/exe` (which would resolve
the extracted mount point, not the AppImage itself).

## Decision

Implement `install::full::linux` with an API symmetrical to `install::full::macos`:

| Function | macOS | Linux |
|---|---|---|
| `current_app_bundle()` | Derives `.app` from `exe` ancestors | `$APPIMAGE` env var → `PathBuf`; fallback to `/proc/self/exe` |
| `extract_app_bundle(archive, into)` | `tar.gz` → `Contents/` directly | `tar.gz` → directly (same strip-leading trick) |
| `install(archive, app_path)` | Extracts to tmp, two-step rename, `osascript` on `PermissionDenied` | **AppImage path**: same two-step rename (no escalation — same-user ownership assumed). **deb path**: delegates to `dpkg -i`, returns immediately. |
| `relaunch(app_path)` | `open -n` | **AppImage path**: `execve` of `app_path` (replaces process). **deb path**: returns `Ok(())` — caller does `app.exit(0)`. |

### AppImage primary path

1. Resolve current bundle from `$APPIMAGE`.
2. Extract downloaded `.tar.gz` (contains `AppName.AppImage` + `.desktop` files under a leading `AppName/` prefix) into a same-device temp dir.
3. Two-step rename: current → backup, new → current.
4. `touch` the new binary to refresh mtime.
5. `execve` the new `app_path` via `std::process::Command::new(app_path).spawn()` — Rust's `spawn` maps to `fork+execve` on Unix; the `spawn` call itself returns without error, and the execve replaces the process. Caller must call `app.exit(0)` after `relaunch` returns.

### deb fallback path

`dpkg -i` is called and the function returns `Ok(())`. The next launch of the `.desktop` file will pick up the newly installed deb. This path cannot overwrite the running binary (deb package manager locks it), so fire-and-forget is the only viable strategy.

## Consequences

- TPV gets a cross-platform full-update path callable from a single
  `#[cfg(target_os = "...")]` match in `download_and_install_update`.
- Zero platform-specific code in the caller.
- AppImage in-place rename does **not** work on read-only filesystems
  (CD-ROM, squashfs mounts). The caller must detect `PermissionDenied` on
  `install` and surface a clear error — the AppImage must be on a writable
  volume.
- deb path is fire-and-forget: caller does `app.exit(0)` and trusts the next
  `.desktop` launch. If the user ran the binary directly from `/usr/bin/...`
  (not via `.desktop`), the update is **not** picked up. Trade-off is
  acceptable — TPV is always installed via `.desktop`.
- No privilege escalation (`pkexec`/`sudo`) — TPV AppImage runs as the same
  user who owns it; `PermissionDenied` means the FS is read-only or
  ownership is wrong, not a missing capability.
