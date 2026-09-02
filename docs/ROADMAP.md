<!-- GENERATED from roadmap.model.yml por `axon gen`. La autoridad es el modelo (.yml); NO editar este archivo a mano. -->

# Roadmap — mks-ota

modelo: mks-ota-roadmap (authority=true) · 1 outcomes · 4 tracks · 0 spikes · 8 milestones

## Outcomes

### outcome/ota-crate-verified  ● in_progress
mks-ota — full-cycle verificado en ambas plataformas, sin deuda de payload ni de honestidad de doc
deps: (ninguna)
refs: dec-0001 adr-0045 adr-0046
gate: G1○ G2○ G3○ G4○  (pass=0 provisional=0 partial=0 open=4)
milestones:
  ▸ outcome/ota-crate-verified/minisign-key-rotation-cross-repo-dep  ○ queued — NO_ES_BUG_DE_ESTE_CRATE — verify.rs toma el pubkey como parámetro (&str, 'app-specific, baked in by the caller', comentario en verify.rs:19-21): el crate en sí es agnóstico de clave. El trust anchor bakeado como constante compilada en DOS binarios distintos vive en los REPOS CONSUMIDORES (hub + desktop app), no aquí. La decisión de 'una clave por proyecto o por componente' la está lockeando waxin en el repo del hub — este nodo solo refleja la dependencia cross-repo, NO la decide.

## Tracks

### track/full-install-macos  ✓ done
F1 — verify+download+install full macOS (install::full::macos)
completed: 2026-08-28
deps: (ninguna)
gate: U1✓  (pass=1 provisional=0 partial=0 open=0)

### track/full-install-linux  ● in_progress
AppImage+deb full install simétrico a macOS (ADR-0046) — bloqueado por swap_app_dirs
deps: (ninguna)
gate: U1✓ U2✓  (pass=2 provisional=0 partial=0 open=0)
milestones:
  ▸ track/full-install-linux/swap-appdirs-bug  ○ queued — CODIGO_VIVO - swap_app_dirs (linux.rs:192-212) renombra el directorio extraido entero sobre app_path: el runtime AppImage espera un fichero, recibe un directorio. Test install_swaps_the_bundle_in_place (linux.rs:353-378) sigue #[ignore]d con la justificacion original intacta. El handoff lane-swap-appdirs-2026-09-02.md pedia el fix + quitar el ignore + assert is_file(); VERIFICADO por git log/grep que nunca se aplico - los commits de sib/swap-appdirs son duplicados byte-identicos (mismo mensaje y autor-date, distinto hash por rebase) de los que si mergearon a main via la otra rama, y tratan del path pkexec/deb (THIRD-PARTY.md, examples/pkexec_smoke.rs), no de este bug. df2a891 (dangling, no alcanzable desde ninguna rama) tiene el mismo contenido que 7e58d3a (mergeado): la prueba de que el trabajo del lane nunca ocurrio.

### track/partial-channel  ● in_progress
A/B slot partial updater extraído de tpv-el-haido2 (ADR-0045 D10-D F2) — con deuda documentada
deps: (ninguna)
gate: U1✓  (pass=1 provisional=0 partial=0 open=0)
milestones:
  ▸ track/partial-channel/spa-only-gate  ○ queued — CODIGO_VIVO - stage() (apply.rs:88-147, gate en 132-137) aborta con BadArchive si el zip no trae index.html en la raiz, ANTES de crear el slot. Un binario, un directorio de hooks o una skill no pueden nunca pasar el canal parcial hoy.
  ▸ track/partial-channel/no-permissions-on-extract  ○ queued — CODIGO_VIVO con control positivo - extract_zip (apply.rs:61-88) escribe con File::create+io::copy, nunca lee entry.unix_mode() ni llama set_permissions: ningun extraido sale ejecutable. Grep del crate entero por set_permissions|PermissionsExt|unix_mode|0o755: 2 hits, AMBOS dentro de mod tests de linux.rs (lineas 323,333 - tar header de un fixture sintetico, no una syscall real). Control positivo confirmado: el patron SI encuentra algo, asi que el cero en apply.rs es real.
  ▸ track/partial-channel/mod-rs-overpromise  ○ queued — CODIGO_VIVO - src/install/partial/mod.rs:1-5 (doc-comment del modulo) dice que el canal parcial sirve 'frontend bundle, sidecar, assets, hooks/skills'; el codigo solo admite frontend (bloqueado por spa-only-gate y no-permissions-on-extract). Familia la-exageracion-compila: el doc promete una capacidad que ningun codigo ejecuta.
  ▸ track/partial-channel/bundle-reverted-no-listeners  ○ queued — CODIGO_VIVO (emisor) + NO_VERIFICADO (oyentes) - watchdog.rs:39 declara BUNDLE_REVERTED_EVENT ('ota://bundle-reverted'), emitido en watchdog.rs:121 via app.emit(). Cero listeners dentro de este crate (no le corresponde tenerlos, es una lib). El claim de 'cero oyentes en el consumidor' viene del eje cross-repo del evidence doc (diseno-ota-parcial-034-2026-09-02.md) - NO reverificado por mi en este bootstrap porque el listener, si existe, vive en otro repo (wraith-linux/TPV).
  ▸ track/partial-channel/no-full-cycle-smoke  ○ queued — CODIGO_VIVO - tests/partial_pipeline.rs cubre download->verify->stage->activate->rollback->invalidate en 1 test, pero JAMAS llama a watchdog::reconcile_boot; los 6 tests de watchdog.rs (lineas 140-222) nunca llaman a partial::stage/activate_staged. El ciclo publish->confirmar->sobrevivir-3-arranques->rollback-provocado no lo verifica ningun test - cada mitad se prueba por separado y el pegamento no lo mira nadie.

### track/ci-hardening  ● in_progress
Matriz CI ubuntu+macos (lane-ci-linux-2026-09-02) — verde pero rota contra el comando real del workflow
deps: track/full-install-linux○(in_progress), track/partial-channel○(in_progress)
gate: U1✓  (pass=1 provisional=0 partial=0 open=0)
milestones:
  ▸ track/ci-hardening/cargo-test-broken-on-macos  ○ queued — CODIGO_VIVO, hallazgo propio de este bootstrap - .github/workflows/ci.yml corre 'cargo test' (no --lib) en ambas patas. VERIFICADO en macOS: 'cargo test' -> rc=101, E0433 en examples/pkexec_smoke.rs:18 ('could not find linux in full'), porque ese example llama a mks_ota::install::full::linux sin cfg(target_os), modulo que no existe en macOS. El commit que anadio el example (df2a891/7e58d3a) dice explicitamente 'examples/pkexec_smoke.rs queda fuera de cargo test a proposito' pero no aplico ningun mecanismo real para lograrlo. La verificacion de la lane ci-linux (commit aeb4e6b) contrasto 54 (macOS) vs 57+1 (Linux), que son los numeros de 'cargo test --lib' - el comando que el guard probo no es el que el workflow ejecuta de verdad. Repo 9 commits por delante de origin/main sin push: este workflow roto aun no ha corrido en GitHub Actions.

## Decisiones locked: 1
- dec-0001  Axon bootstrap: SSOT docs/roadmap.model.yml, mutaciones solo vía axon CLI (doc: docs/decisions/dec-0001-axon-bootstrap.md)

## Deferred: 0

## Experimentos adyacentes: 0

---

✓ Sin violaciones de governance derivables.
