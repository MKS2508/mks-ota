---
type: handoff
date: 2026-09-02
lane: ci-linux
roadmapItemId: track/runner-product/ota-linux-nunca-compilo
model: glm
---

# Lane `ci-linux` — este módulo nunca se ha construido para su plataforma

## El hecho, medido

`src/install/full/linux.rs` está tras `#[cfg(target_os = "linux")]` (`mod.rs:7`). Aquí todo
el mundo trabaja en Mac. Resultado: **el `cargo test` del Mac sale verde porque el módulo
entero es invisible.**

Y no era sólo que no se probara — **no compilaba**. Al restaurar el fichero de un commit
anterior y compilarlo en un container Linux:

```
error[E0308]: mismatched types
  --> src/install/full/linux.rs:225:31
      !content.contains(b"old-appimage-content")
      expected `&u8`, found `&[u8; 20]`
error[E0308]: mismatched types   (:228, mismo patrón)
```

Su módulo de tests se escribió y se commiteó roto, y **nadie podía enterarse**. La rama deb
del OTA —instalar y actualizar en Linux— nunca se ha construido para su plataforma objetivo.
Los dos errores ya están arreglados (commit `0a6f05d`); lo que falta es que esto **no pueda
volver a pasar**.

## Lo que este lane entrega

CI que ejecuta la suite **en Linux**, además de en macOS. Es barato: un container `rust` y
`cargo test`. Literalmente lo que hice yo para descubrirlo — 57 passed, 0 failed en
linux/amd64.

Este repo **no tiene `.github/` todavía**: lo creas tú.

## Requisitos

- **Matriz de al menos dos plataformas**: `ubuntu-latest` y `macos-latest`. El punto entero
  del lane es que una sola no basta.
- Debe correr **`cargo test`** (no sólo `cargo build`) y **`cargo clippy`** si el repo ya lo
  usa — mira antes si hay config, no lo impongas.
- La versión de Rust: **míralo en `Cargo.toml`/`rust-toolchain.toml`** y pínala. No pongas
  `stable` flotante en un repo que produce un instalador.
- Rápido: cachea `~/.cargo` y `target/` con `actions/cache` o equivalente.

## Falsabilidad — obligatoria, y aquí es fácil y sin excusa

El workflow tiene que **fallar en rojo** ante exactamente el bug que motivó el lane.

1. **ROJO**: reintroduce a mano uno de los dos `E0308` (p.ej. cambia un
   `contains_subslice(&content, b"...")` de vuelta a `content.contains(b"...")`) y demuestra
   que **el job de Linux falla**. Guarda la salida literal.
2. **VERDE**: revierte y demuestra que pasa.
3. **Y la parte que de verdad importa**: demuestra que **el job de macOS pasa igualmente con
   el bug metido** — porque el módulo es invisible ahí. Eso es lo que prueba que la matriz
   aporta algo. Si no lo enseñas, el lane no ha demostrado su propia razón de ser.

**No necesitas GitHub Actions corriendo de verdad para (1) y (2)**: puedes ejecutar el mismo
`cargo test` en un container Linux (tienes ssh a `vps-helsinki`, Ubuntu 24.04 con docker) y en
el Mac. Lo que se pide es la evidencia de los tres resultados, no un run verde en la web.
Si además consigues un run real, mejor — dilo.

⚠️ `omarchy-vm` está **apagada**. No cuentes con ella.

## Fronteras

| no toques | por qué |
|---|---|
| `src/**` | lo posee el lane `swap-appdirs`, EN VUELO en paralelo |
| `Cargo.toml` / `Cargo.lock` | si crees que hace falta tocarlos, **para y repórtalo** |

**No arregles bugs que encuentres.** Este lane construye el instrumento. Si el CI destapa algo
más —y es probable—, lo listas en el report y sigues. Un lane que arregla lo que su propio
instrumento descubre no puede decir si el instrumento funciona.

## STOP explícito

Si no consigues las tres evidencias (rojo en Linux, verde en Linux, **verde en macOS con el
bug puesto**), **no declares done**. Di cuál falta, qué intentaste y por qué. Un CI entregado
como «verificado» sin haberlo visto en rojo es exactamente el tipo de guard que este repo
acaba de descubrir que tenía.

## Report

`/tmp/ci-linux-report.md`, **antes de terminar** — obligatorio. El return value de un agente
se pierde; el fichero sobrevive. Un lane anterior de este mismo repo se quedó idle sin
escribirlo y hubo que reconstruir su trabajo entero a mano.
