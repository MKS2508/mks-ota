---
type: decision
id: dec-0001
status: locked
date: 2026-09-03
locked-by: waxin
scope: [orchestration, tooling]
governs: [roadmap.model.yml, ROADMAP.md]
refs: [adr-0045, adr-0046]
---

# dec-0001 — Axon bootstrap: SSOT centralizado en docs/ + import de lo ya diseñado

## Contexto

`mks-ota` no tenía SSOT: solo `docs/adr/` (1 ADR) y `docs/handoffs/` (3 handoffs), sin ningún
doc de estado que atara el trabajo entregado (full-install macOS, canal parcial extraído,
full-install Linux, matriz CI) a lo que sigue pendiente. La síntesis del workflow
`diseno-ota-parcial-034-2026-09-02.md` (6 ejes + sintetizador, mks-agentics) ya había hecho el
diagnóstico completo de este crate — vivía disperso en otro repo, sin absorber.

## Decisión

1. **Layout: idéntico al de `desktop-release-hub`** (repo hermano que ya usa este patrón).
   `docs/roadmap.model.yml` = autoridad · `docs/ROADMAP.md` = generado (header GENERATED,
   nunca editar) · `docs/decisions/` · `docs/handoffs/` (ya existía) ·
   `.claude/axon.config.json` (`roadmapModel`).
2. **Alcance del modelo inicial: import del trabajo ya hecho + los 8 hallazgos ya
   diagnosticados**, cada uno verificado línea por línea en el código de este repo antes de
   entrar al modelo (no se convirtió ningún `SOLO_DOC` del evidence doc en hecho sin
   re-verificarlo aquí). Un hallazgo (CI roto en macOS por `examples/pkexec_smoke.rs`) es
   propio de este bootstrap, no venía en la síntesis previa.
3. **Mutaciones SOLO vía axon CLI** (`bunx @mks2508/axon` ≥0.2.2:
   `set-status`/`set-gate`/`add-gate`/`add-node`/`rm-node`/`set-node`). Edición manual del
   `.yml` prohibida salvo las excepciones documentadas en el evidence del bootstrap (ver
   abajo) — todas cosméticas (blank lines, comentarios de gate), nunca contenido semántico.
4. **Commit único del setup, sin push.** El push de cualquier rama requiere OK explícito de
   waxin en el mismo turno.

## Reglas que gobierna

- **Mutaciones del modelo SOLO vía axon CLI.** Si la tooling tiene un bug: flag + ticket +
  fallback manual documentado en esa sesión (ver limitaciones de la CLI, abajo).
- **`docs/ROADMAP.md` jamás se edita a mano** — se regenera (`axon gen --out docs`). El guard
  `bunx @mks2508/axon gen --out docs --check` falla (exit 2) si el doc diverge del modelo.
- **El doc generado nunca puede estar más current que el modelo** — si hay que cambiar estado:
  mutar el modelo primero, regenerar después.

## Limitaciones verificadas de `@mks2508/axon` 0.2.2 (para el próximo que use esta CLI aquí)

Encontradas y confirmadas con reproducción aislada durante el bootstrap:

- **Bootstrap desde archivo vacío es imposible**: `nodes: []` y `nodes:` (null) rompen
  `add-node` (`seq vacía o sin map final` / `nodes must be an array`). Hace falta un nodo
  semilla escrito a mano como primer nodo — inevitable, análogo a `git init`.
- **Las decisiones (`decisions:`) no son gestionables por la CLI**: `add-node --status locked`
  falla porque el enum de `--status` es el de nodos normales (`queued, in_progress, done,
  conditional, vision-locked`) y no incluye `locked`. Las entradas de `decisions:` se escriben
  a mano en el bootstrap (una vez, igual que hace `desktop-release-hub`) y no se vuelven a
  tocar después (son registros append-only).
- **Los `gate.items` no tienen campo de descripción/título** en el schema — solo
  `id/verdict/class/evidenceRef/verifiedBy/owner`. Para gates a nivel `outcome` que necesitan
  contexto legible (G1-G4 aquí), la única vía es un comentario YAML bajo el item — igual que
  hace `desktop-release-hub`. Es hand-edit puramente cosmético (no toca ningún campo del
  schema), no una mutación de estado.
- **`add-node --parent <X>` (crear milestone) falla siempre que `<X>` ya tenga un bloque
  `gate:`** (`nodo no recuperable tras mutación`), sin importar la posición del nodo ni el
  número de items del gate. Reproducido de forma aislada 3 veces. Workaround usado aquí:
  añadir milestones ANTES de añadir gate items a un track (si un track ya tenía gate,
  `rm-node --force` + recrear en el orden correcto).
- **Insertar contenido nuevo (milestone o gate) en el nodo que ocupa la última posición de
  `nodes:`** rompe la detección del límite con la siguiente clave top-level (`decisions:`),
  con errores variados (`All mapping items must start at the same column`, `nodo no
  recuperable`). Workaround: mantener un nodo trailing "sentinel" sin necesidad de mutación
  mientras se opera sobre los demás, borrarlo al final con `rm-node --force`.
- **Evidence refs que parsean como número científico**: un SHA corto tipo `23939e5` (dígitos +
  `e` + dígito) se interpreta como float en notación E (`must be a string (was a number)`).
  Workaround: usar SHAs de ≥10-12 caracteres.
- **`add-node`/`add-gate` a veces omiten la línea en blanco de separación** antes del
  siguiente `- id:` top-level, dejando el YAML sintácticamente inválido para la SIGUIENTE
  escritura (aunque las lecturas —`status`/`gen`— lo toleran). Hubo que insertar 5 líneas en
  blanco puntuales durante el bootstrap, cada una verificada con `axon status` antes de seguir.

Ninguna de estas limitaciones se arregló aquí (no es scope de este repo — `@mks2508/axon` es
una dependencia global). Quedan documentadas para que el próximo bootstrap con esta CLI no
las redescubra a ciegas.
