# mks-ota

A Rust updater crate for Tauri apps, hub-agnostic and reusable across separate repos. Not a
Tauri plugin — the hub does not speak `tauri-plugin-updater`'s manifest shape, and this crate
does not adopt it either (ADR-0045 D8-B).

## Who consumes this

Two apps in two different repos: `mks-agentics/apps/wraith-linux` and `tpv-el-haido2`. The API
is shaped by having two real consumers from day one, not by one app's needs extracted later.

## Status (F1)

Full install only — the whole app package, replaced and relaunched. macOS only. Linux
(AppImage) and the partial-install path (a single artifact swapped without reinstalling the
app) are declared as module holes (`src/install/full/linux.rs`, `src/install/partial.rs`) but
not implemented yet — see `docs/jarvis/ota-crate-design-2026-08-28.md` in `mks-agentics` for the
full design and F2/F3 scope.

## Modules

```
manifest              deserializes the hub's own `GET /api/components/{c}/latest` JSON
verify                minisign-verify: content signature + trusted-comment global signature
download              stream to a temp file, sha256 while streaming, Range/resume
install::full::macos  extract + swap the .app in place, relaunch
```

## Adding as a dependency

```toml
[dependencies]
mks-ota = { git = "ssh://git@github.com/MKS2508/mks-ota", tag = "v0.1.0" }
```

The pubkey, the hub URL, and which artifact classes to request are the caller's responsibility —
this crate has no baked configuration.

## Third-party code

`src/install/full/macos.rs` derives from `tauri-plugin-updater` 2.10.1 (`Apache-2.0 OR MIT`).
See `THIRD-PARTY.md` for the exact attribution and what was and wasn't carried over.
