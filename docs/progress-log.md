# Progress log — mks-ota

Entradas **más recientes arriba**. Provenance (qué pasó y con qué evidencia), NO estado
corriente — el estado vive en `roadmap.model.yml` (SSOT). Entrada por milestone lockeado.

---

## 001 — 2026-09-03 — Bootstrap SSOT: import de deuda verificada, cero cambio de código

**Qué**: bootstrap del patrón axon (replicado de `desktop-release-hub`) sobre un repo que
solo tenía `docs/adr/` y `docs/handoffs/` sueltos. Se cargó el modelo con el trabajo ya
mergeado (full-install macOS/Linux, canal parcial extraído, matriz CI) y 8 milestones
verificados línea por línea contra el código de este repo — no copiados a ciegas del
evidence doc externo (`diseno-ota-parcial-034-2026-09-02.md`, mks-agentics).

Hallazgo más relevante: **el lane `swap-appdirs` nunca aplicó su fix** — verificado con
`git log`/`git show --stat`, no solo leído el handoff. Segundo hallazgo, propio de este
bootstrap: el `.github/workflows/ci.yml` recién construido rompe en la pata macOS porque
`cargo test` (no `--lib`) intenta compilar `examples/pkexec_smoke.rs` sin
`cfg(target_os = "linux")`.

Commit: `7ad91afd1c6259e7a1fa0e0ee437e01356cb4e2c`. Sin push (requiere OK explícito de
waxin). `src/` no se tocó — confirmado con `git status` y `cargo test --lib` idéntico
antes/después (54 passed).
