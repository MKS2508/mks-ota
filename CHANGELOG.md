# Changelog

Versions are git tags (`vX.Y.Z`); consumers pin a `rev`/`tag` (see README).

## 0.4.0 — unreleased

Push channel, urgency and an unattended-safe full update (TR-30 of `tpv-el-haido2`).

### Added
- `manifest::Urgency` (`normal` | `urgent`) and `HubLatest::urgency()`. The raw field is
  `HubLatest.urgency: Option<String>`; absent, `null`, unknown or non-string values read as
  `Normal`, so a level added by a newer hub never breaks the client.
- `SlotState.staged_urgency`: written by `install::partial::stage` from `/latest`, cleared
  together with `staged_version` (activation, native invalidation). Old `state.json` files load
  unchanged; an unknown stored value reads as `Normal` instead of making the file "corrupt".
- `push`: optional SSE subscription to the hub's `GET /api/events?components=…`.
  `push::subscribe(PushConfig, FnMut(PushSignal))` → `PushHandle` (`stop()` joins; drop stops
  without waiting). Signals are `Connected` (query `/latest` now: the hub does not replay) and
  `Published { component, version, urgency }` — a hint, never a `HubLatest`. Exponential backoff
  with full jitter (1 s → 300 s by default, reset only after a connection lived 60 s), `retry:`
  as the minimum delay, `Last-Event-ID` on reconnect, 40 s read timeout. One `std` thread per
  subscription, blocking client built inside it, no global state, no new dependencies.
- `download::download_controlled` + `DownloadControl { max_bytes_per_sec, should_pause }`:
  average rate limit, and pause = close the connection and return `OtaError::Paused` with the
  prefix kept for a `Range` resume. `download` is `download_controlled` unlimited.
- `install::full::trial` (Linux): `begin` keeps the running AppImage as `<app>.ota/previous`
  (hardlink, or an fsynced copy renamed into place), `on_boot` counts boots and returns
  `MustRevert` past `MAX_FULL_BOOTS` (2), `mark_ready`, `revert`, `reverted_version`,
  `trial_version`, `FULL_READY_GRACE` (120 s). Plain one-line files with atomic writes, a
  contract shared with shell launchers (documented in the module).
- `OtaError::Paused`, `OtaError::NothingToRevert`.

### Fixed
- `download`: a sha256 mismatch now deletes `dest`. The bad file used to stay on disk and every
  later attempt resumed on top of it with `Range` and failed forever.
- `download`: a `416` on a resume (prefix as long as the remote file) restarts from zero once
  instead of failing.

### Also in this release (merged after 0.3.0)
- The hub's native window (`nativeMin`/`nativeMax` in `/latest`) gates partial staging.
- Partial: a staged artifact expires when the native binary changes; `activate_staged` refuses a
  slot that does not know which native it was staged for; a rollback restores the previous slot
  with its version and is not undone by the next poll.
- Full/Linux: the AppImage is replaced atomically (raw or from a `.tar.gz`) and `relaunch` execs
  the new AppImage in place.

### Compatibility
- Wire-compatible: new manifest and state fields are optional with defaults.
- Source-level: `HubLatest` and `SlotState` gained public fields and `OtaError` gained variants.
  Code that builds `HubLatest { … }` without `..` or matches `OtaError` exhaustively needs the
  new fields/arms (`SlotState` literals with `..Default::default()` are unaffected).

## 0.3.0 — 2026-08-29
- `install::full::linux`: AppImage and `.deb` full install (ADR-0046).

## 0.2.0 — 2026-08-29
- `install::partial`: A/B slot updater extracted from `tpv-el-haido2` (ADR-0045 D10-D F2), with
  the `tauri`-gated `protocol` and `watchdog`.

## 0.1.0 — 2026-08-28
- `manifest`, `verify` (minisign), streaming `download` with sha256 and `Range` resume, and
  `install::full::macos`.
