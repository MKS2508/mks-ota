---
type: handoff
unit: lane-haido-full-update-phase-a-linux
target_lane: mks-ota-install-full-linux
status: queued
created: 2026-08-29
priority: P1 (desbloquea TPV full update Linux)
base: main (HEAD = 8df4138, v0.2.0 ya pusheado)
repo: MKS2508/mks-ota
executor: task-executor
orchestrator: axon-v2
---

# lane-haido-full-update-phase-a-linux

Materializar el **module hole** `mks_ota::install::full::linux` que `lib.rs:7`
declara y que `install/full/linux.rs` documenta como "F2 scope, not implemented
in F1" (11 líneas de doc-comment, 0 código). El API debe ser **simétrico** a
`install::full::macos` (4 funciones con misma firma), porque `tpv-el-haido2`
lo va a llamar desde un solo site (lib.rs `download_and_install_update`) con
un `#[cfg(target_os = "...")]` match.

## TL;DR

Implementar 4 funciones públicas en `src/install/full/linux.rs` con la misma
firma que `src/install/full/macos.rs`. Tests en el mismo módulo. ADR-0046
documenta el shape y el por qué. Bump `0.2.0 → 0.3.0` (API nueva, semver
minor). Push + tag `v0.3.0` (pre-push; waxin OK explícito en el turno del
orchestrator).

## Contexto — qué decidió waxin

- **Lane raíz**: "implementa el full y todo lo que falte de linux hazlo
  simétrico a mac" (waxin, 2026-08-29).
- **Lane previa** (cleanup 2523408 en tpv-el-haido2) mató el canal OTA full
  via `tauri-plugin-updater` por endpoint 404 desde M6.
- **Lane previa** (commit 8df4138, ya pusheado a mks-ota main como v0.2.0)
  materializó la extracción partial (install::partial::{apply,slots},
  watchdog, manifest::partial_latest_url). 54 tests verdes pre-tag.
- **Esta lane** materializa el lado Linux del full update simétrico a macOS.

## Scope (qué hacer)

### 1. `src/install/full/linux.rs`

Reemplazar el contenido del hole (11 líneas) con la implementación. Estructura
exacta:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Portions derived from `tauri-plugin-updater` 2.10.1 — see THIRD-PARTY.md.
//
// AppImage full-install — extracts the downloaded archive in place using the
// running executable's `$APPIMAGE` env var, then renames the new artifact on
// top and re-execs the new AppImage. The deb flavor falls back to `dpkg -i`
// for installations that ran from a deb package — the running process
// can't overwrite its own deb on disk (privilege + lock constraints), but
// the next launch picks up the new version installed via dpkg.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use tar::Archive;
use tempfile::TempDir;

use crate::error::OtaError;

/// Resolves the current `.AppImage` path from `$APPIMAGE` (set by the
/// AppImage runtime when launching the bundled binary, see
/// `tauri-plugin-updater` `src/updater.rs:101-113`). Falls back to
/// `/proc/self/exe` for deb installs (Tauri sets the binary path on
/// `tauri::Builder`, but reading from the kernel procfs is the same trick
/// `updater.rs:1424` uses for the macOS `current_app_bundle` shape).
pub fn current_app_bundle() -> Result<PathBuf, OtaError> { /* … */ }

/// Extracts `archive` (a `.tar.gz` containing the new `.AppImage` and
/// `.desktop` files under a leading `AppName/` directory) into `into`,
/// skipping the leading path component — same shape as the macOS
/// counterpart.
pub fn extract_app_bundle(archive: &Path, into: &Path) -> Result<(), OtaError> { /* … */ }

/// Downloads-and-verified install: extracts the archive next to the running
/// AppImage, then renames the new AppImage on top of the current one
/// (same-device check, no `osascript` fallback — Linux is POSIX and the
/// AppImage is owned by the same user that runs it, no privilege
/// escalation needed). If the running bundle was a deb (no `$APPIMAGE`),
/// this delegates to `dpkg -i` and returns — the new binary takes effect on
/// the next launch.
pub fn install(archive: &Path, app_path: &Path) -> Result<(), OtaError> { /* … */ }

/// Relaunches the app at `app_path` via `execve` of the new AppImage
/// (AppImage case) or returns Ok(()) for the deb case (dpkg already queued
/// the new version, the caller is expected to exit so the next launch
/// picks it up).
pub fn relaunch(app_path: &Path) -> Result<(), OtaError> { /* … */ }
```

**Decisiones de diseño a respetar (ya lockeadas en conversation con waxin):**

1. **AppImage es el primary path** (TPV bundlea AppImage; wraith-linux también).
   deb es fallback para `dpkg -i`-installed builds.
2. **No privilege escalation** (no `pkexec` / no `sudo`) — TPV corre como usuario
   normal y su AppImage vive en el home del usuario. Si el rename falla con
   `PermissionDenied`, devolver `OtaError::PrivilegedInstallFailed` con mensaje claro,
   no escalar.
3. **Same-device check antes del rename** (como macos.rs `pick_tmp_dir`) — el
   mismo gotcha que el plugin upstream tiene en Linux pero no en macOS.
4. **Rollback atómico** — `swap_app_dirs` análogo a macos.rs:99-116. Si el
   segundo rename falla, restaurar el backup antes de devolver el error.
5. **El caller hace `app.exit(0)` después de `relaunch`** (mismo shape que
   `wraith-linux/src-tauri/src/lib.rs:875`). No hacer `process::exit` aquí.

### 2. Tests (mismo módulo)

Replicar los 4 tests de macos.rs:189-254 con adaptaciones a Linux:

- `extracts_dropping_the_leading_app_component` — fixture real de un
  AppImage `.tar.gz` publicado por el hub (`haido.releases.mks2508.systems`
  o un fixture local en `tests/fixtures/`). Si no hay fixture real accesible,
  construir uno sintético en `TempDir` con un header `AppName.appimage` + el
  binario adentro, guardarlo, y usar ese en TODOS los tests.
- `install_swaps_the_bundle_in_place` — crear un AppImage fake en
  `tempdir`, instalar encima, verificar que el marker file viejo ya no está
  y el nuevo sí.
- `relaunch_invokes_execve_without_erroring_synchronously` — igual que el
  macos: spawn no falla síncronamente, el execve real lo verifica otro
  proceso.
- `a_failed_swap_restores_the_previous_bundle` — forzar segundo rename a
  fallar, verificar rollback.

### 3. ADR-0046 — `docs/adr/adr-0046-linux-full-install.md`

Contenido mínimo:

- **Status**: proposed (lock se hace al mergear)
- **Context**: TPV es Linux-first + macOS. Después de matar el
  `tauri-plugin-updater` (commit `2523408`), el canal full OTA solo existe
  para macOS vía `install::full::macos`. Linux queda sin full update.
- **Decision**: implementar `install::full::linux` con API simétrica a
  macos. AppImage via `$APPIMAGE` + rename in-place (primary), deb via
  `dpkg -i` (fallback).
- **Consequences**:
  - ✅ TPV cross-platform full update (siguiente lane)
  - ✅ Cero código platform-specific en el caller (un solo `match target_os`
    en `download_and_install_update`)
  - ⚠️ AppImage in-place rename NO funciona si la AppImage está en un FS
    read-only (CD-ROM, squashfs montado). El caller debe detectar y avisar.
  - ⚠️ deb path es fire-and-forget — el caller hace `app.exit(0)` y
    confia en que el siguiente launch del .desktop file coja el deb nuevo.
    Si el usuario tenía el binario corriendo desde `/usr/bin/...` directamente
    (no desde .desktop), NO se actualiza. Trade-off aceptable: TPV se
    instala vía .desktop file siempre.

### 4. THIRD-PARTY.md

Si portas código de `tauri-plugin-updater` `src/updater.rs` (mirar el de
macos.rs:1-9 para el estilo exacto del header), añadir entrada en
`THIRD-PARTY.md` debajo de la sección macOS. Mismo attribution header en el
`.rs`.

### 5. Bump version + commit

- `Cargo.toml`: `version = "0.2.0"` → `version = "0.3.0"` (semver minor — API
  nueva).
- Commit message (sin co-author, sin AI attribution):

  ```
  feat(install::full::linux): AppImage + deb full install (ADR-0046)

  Implementa el module hole `install::full::linux` con API simétrica
  a `install::full::macos` (current_app_bundle, extract_app_bundle,
  install, relaunch). AppImage es el primary path via $APPIMAGE env
  + rename in-place + execve del nuevo binario; deb es fallback via
  `dpkg -i` (fire-and-forget, next-launch picks up).

  Tests: 4 unit tests con TempDir + fixture sintético de AppImage
  (replica los 4 macos.rs:189-254 adaptados). Sin fixture externo;
  toda la suite corre en ~1s sin red.

  Attribution: `tauri-plugin-updater` 2.10.1 `src/updater.rs:101-113`
  (APPIMAGE env lookup) y `:1064` (same-device check), per
  THIRD-PARTY.md.

  Desbloquea la lane `lane-haido-full-update-phase-b-tpv` que wirea
  TPV para usar install::full::{macos,linux} cross-platform.

  Refs: docs/adr/adr-0046-linux-full-install.md
  ```

## NO TOCAR (out of scope)

- `src/install/full/macos.rs` — intacto, es el template a seguir
- `src/install/partial/` — extraído en commit 8df4138, intacto
- `src/watchdog.rs`, `src/protocol.rs`, `src/manifest.rs` — extraídos en
  commit 8df4138, intactos
- `src/lib.rs` — no requiere cambios (linux.rs ya está en
  `install/full/mod.rs:5` con `#[cfg(target_os = "linux")]`)
- `tests/` integration tests — no tocar (los unit tests viven en el módulo)
- `THIRD-PARTY.md` solo si portas código upstream; si todo es código nuevo,
  no hace falta entrada.

## Trade-off conocido

`install::full::linux::relaunch` para AppImage hace `execve` del nuevo
binario. Esto **reemplaza el proceso actual**, no spawna uno nuevo. El caller
DEBE no tener código que corra después de `relaunch` (return type es Result,
no Never). El caller en TPV/wraith-linux ya hace `app.exit(0)` justo después
de `relaunch` (lib.rs:875), lo cual mata el proceso antes de que el
execve pueda fallar — eso es OK porque si execve falla, el exit ya está
pedido. NO cambiar este shape.

## Verificación (corredor tú mismo)

```bash
# 1. Compile limpio
cargo check
# Expected: 0 errors, 0 warnings

# 2. Tests
cargo test --lib install::full::linux
# Expected: 4 passed (extracts_dropping_the_leading_app_component,
#                       install_swaps_the_bundle_in_place,
#                       relaunch_invokes_execve_without_erroring_synchronously,
#                       a_failed_swap_restores_the_previous_bundle)

# 3. Suite completa (no regresión)
cargo test --lib
# Expected: 54 + 4 = 58 passed

# 4. Sin co-author, sin AI attribution
git log --format='%an <%ae>' -1
# Expected: tu nombre real, no "Claude" / "AI"
git log --format='%(trailers)' -1
# Expected: vacío
```

## Report contract

Persiste tu reporte a `/tmp/lane-haido-full-update-phase-a-report.md`
(waxin lock 2026-08-18: TODO agente spawneado persiste a fichero ANTES de
terminar — el return value del agente se pierde; el fichero sobrevive).
Schema axon-artifacts estándar:

```yaml
---
type: report
unit: lane-haido-full-update-phase-a-linux
status: completed | needs-iteration | blocked
verdict: closed | closed-with-deferred | needs-iteration
---

## Resumen
<1-3 líneas>

## Cambios
- <file>: <qué cambió>

## Verificación (con evidencia)
- cargo check: <resultado>
- cargo test --lib install::full::linux: <4/N passed>
- cargo test --lib: <58/N passed>
- git log -1 --format='%an <%ae>': <sin AI>

## Trade-offs / Deferred
- <si algo quedó diferido>

## Veredicto
<closed | needs-iteration> — <1 línea razón>
```

NO commitees ni pushees — el orchestrator hace el push tras verificar tu
reporte independientemente y obtener OK explícito de waxin.

## STOP conditions

Para y reporta si:

- `install::full::macos` no compila en clean (regresión de tu lado)
- Algún test macOS falla (regresión)
- El fixture sintético que necesitas > 1MB y preferirías uno real — pregunta
  antes de hacer commit con un fixture pesado en el repo
- `cargo check` para `target_os = "linux"` no se puede verificar desde
  macOS (es normal, no es blocker — el ejecutor es macOS, pero Rust compila
  cross-platform sin problema)