---
type: handoff
date: 2026-09-02
lane: swap-appdirs
roadmapItemId: track/runner-product/swap-app-dirs-rompe-appimage
model: minimax
---

# Lane `swap-appdirs` — el update de AppImage deja un directorio donde debe haber un fichero

## El bug

`swap_app_dirs` renombra el **directorio extraído entero** sobre `app_path`, así que
`app_path` acaba siendo un **directorio** en vez del fichero `.AppImage` que el runtime
espera.

Verificado, no leído: el test `install_swaps_the_bundle_in_place` **falla de verdad**
(`panicked at src/install/full/linux.rs:369`) — está apagado con `#[ignore]` y una
justificación larga dentro del atributo.

**Y no es menor**: el AppImage es *el* artefacto de distribución y de OTA en Linux
(`install.sh` lo baja del hub, `mks-ota` se auto-sobrescribe). Si el update deja un
directorio donde iba el ejecutable, **deja la instalación rota**.

## Por qué llevaba tiempo escondido

El módulo está tras `#[cfg(target_os = "linux")]` y aquí se trabaja en Mac, así que ni
compilaba — dos `E0308` en su propio módulo de tests. Arreglar la compilación (commit
`0a6f05d`) dejó este bug a la vista. Antes el test que lo caza **no podía ni construirse**.

## Lo que entregas

1. `swap_app_dirs` arreglado: tras un install, `app_path` es **el fichero `.AppImage`**, con
   el bit `+x`, y su contenido es el nuevo.
2. El `#[ignore]` **borrado** y el test verde. Con su justificación larga fuera: si el código
   se defiende solo, la apología sobra.

Mira cómo lo hace `macos.rs` antes de inventar nada — el caso de macOS es un `.app`, que sí
es un directorio, y **esa asimetría es probablemente el origen del bug**. Un camino
compartido que asume «bundle = directorio» y en Linux no lo es.

## Falsabilidad — las dos ramas

- **VERDE**: `install_swaps_the_bundle_in_place` pasa sin `#[ignore]`, y el assert comprueba
  que `app_path` **es un fichero** (`is_file()`), no sólo que su contenido cambió. El test
  actual no lo comprobaba, y por eso el bug cabía.
- **ROJO**: revierte tu fix y enseña que el test **vuelve a fallar**. Un test que nunca has
  visto en rojo no prueba que tu cambio haga algo.

## Dónde ejecutarlo — no es opcional

**En el Mac este módulo no compila siquiera.** Tienes ssh a `vps-helsinki` (Ubuntu 24.04 con
docker). Receta que ya funciona:

```
docker run -d --name <tuyo> rust:1.98-slim sleep 3600
docker cp <src> <tuyo>:/src
docker exec <tuyo> bash -c 'export PATH=/usr/local/cargo/bin:$PATH; cd /src && cargo test --lib'
```

Baseline de referencia: **57 passed, 0 failed, 1 ignored** (el ignored es justo el tuyo).
Cuando acabes deben ser **58 passed, 0 ignored**.

⚠️ `omarchy-vm` está **apagada**. No cuentes con ella.

## Fronteras

| no toques | por qué |
|---|---|
| `.github/**` | lo posee el lane `ci-linux`, EN VUELO en paralelo |
| `install_deb_with_pkexec` / `resolve_deb_path` | recién mergeado y verificado, no es tuyo |
| `macos.rs` | si crees que el fix correcto obliga a tocarlo, **para y repórtalo** — eso es un cambio de diseño compartido, no un bugfix |

## STOP explícito

Si el fix te pide cambiar la firma de algo que `macos.rs` también usa, **para**. Y si te
encuentras escribiendo un comentario largo explicando por qué tu solución está bien así: ese
comentario es la señal de que no lo está. Borra la apología y arregla el código, o para y
repórtalo.

## Report

`/tmp/swap-appdirs-report.md`, **antes de terminar** — obligatorio. Incluye el conteo de tests
antes y después, y la salida literal de la rama roja.
