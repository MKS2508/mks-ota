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
manifest              deserializes the hub's own `GET /api/components/{c}/latest` JSON (+ urgency)
verify                minisign-verify: content signature + trusted-comment global signature
download              stream to a temp file, sha256 while streaming, Range/resume,
                      rate limit + pause (download_controlled)
install::full::macos  extract + swap the .app in place, relaunch
install::full::linux  AppImage (atomic swap) + deb (pkexec dpkg)
install::full::trial  previous AppImage kept aside, boot counting, revert (Linux, AppImage only)
install::partial      A/B slots in appDataDir: stage/activate, rollback
policy                decide(urgency, ApplyKind{Hot,Restart}): ask / apply-when-idle — normal
                      asks; urgent auto-applies Hot, asks for Restart; forced auto-applies both.
                      Only through an app IdleHook (NeverIdle by default); when_idle
push                  optional SSE subscription to the hub's /api/events — a hint to
                      query /latest now, never an update; reconnects with backoff + jitter
```

See `CHANGELOG.md` for what each version added.

## Adding as a dependency

```toml
[dependencies]
mks-ota = { git = "ssh://git@github.com/MKS2508/mks-ota", tag = "v0.1.0" }
```

### Tauri 2 or Tauri 3

The Tauri-bound modules (`protocol`, `watchdog::arm_hot_apply`) sit behind the `tauri` feature.
Which Tauri major that means depends on the package:

```toml
# Tauri 2
mks-ota = { git = "ssh://git@github.com/MKS2508/mks-ota", tag = "v0.6.0", features = ["tauri"] }

# Tauri 3 (pinned to tauri =3.0.0-alpha.6): same source, same `mks_ota` crate name
mks-ota = { package = "mks-ota-tauri3", git = "ssh://git@github.com/MKS2508/mks-ota", tag = "v0.6.0", features = ["tauri"] }
```

They are two packages, not two features, because `tauri` declares `links = "Tauri"` and one
manifest cannot name both majors (see CHANGELOG 0.6.0). Code that must differ between majors
uses `#[cfg(feature = "tauri-v3")]`. Test both with:

```sh
cargo test --features tauri                                    # Tauri 2
cargo test --manifest-path tauri3/Cargo.toml --features tauri  # Tauri 3
```

The pubkey, the hub URL, and which artifact classes to request are the caller's responsibility —
this crate has no baked configuration.

## Third-party code

`src/install/full/macos.rs` derives from `tauri-plugin-updater` 2.10.1 (`Apache-2.0 OR MIT`).
See `THIRD-PARTY.md` for the exact attribution and what was and wasn't carried over.
