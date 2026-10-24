# gpuix-qjs

A QuickJS-ng host for GPUIX apps: one executable that links `gpuix-native` (the GPUI renderer,
as a Rust library rather than a `.node` addon) with the QuickJS-ng engine through
[rquickjs](https://github.com/DelSkayn/rquickjs). It runs a bundled app with no Node and no Bun,
in about 18 MB on macOS arm64, almost all of it GPUI. [gpuix-svelte](https://github.com/khromov/gpuix-svelte)
drives it as `gpuix-svelte --qjs`.

```bash
cargo build --release                       # → ../native/target/release/gpuix-qjs (shared target dir)
gpuix-qjs dist/app/main.js [args]           # run a bundle; a line `reload` on stdin re-imports it
gpuix-qjs --pack dist/app --entry main.js --out dist/myapp   # one executable with the bundle inside
gpuix-qjs --version
```

## What the host provides

- `@gpuix/native` as a native module exporting `GpuixRenderer` with the napi class's method names
  (`init`, `applyBatch`, `tick`, `getElementBounds`, `promptForPaths`, `captureScreenshot`, ...),
  plus `isTerminal` and `hasTestGpuixRenderer` (`false`: no test renderer here). The constructor
  takes the same `(err, event)` callback; GPUI raises events inside `tick()`'s AppKit pump into a
  queue and `tick()` delivers them before returning. `setFrameWakeCallback` answers `false`, so the
  JS frame loop stays a paced `setTimeout` chain, as on Windows.
- Globals a GPUIX renderer uses: `console`, `setTimeout`/`setInterval` and their `clear*`,
  `process` (`env`, `argv`, `platform` in Node's spelling, `exit`, `cwd`, `stdout`/`stderr.write`,
  `nextTick`, `versions.quickjs`), and polyfills of `queueMicrotask` and `structuredClone` where
  the engine lacks them. `performance.now` is QuickJS-ng's own.
- Module loading from disk by absolute path, relative specifiers resolved against the importer. A
  `?query` is part of a module's identity, which is how a dev server's `?v=N` re-import gets a
  fresh instance of the app while its unchanged chunks stay cached.
- The process is the frame loop: `main` runs due timers and pending promise jobs until the app
  calls `process.exit`, which the JS loop does when `tick()` reports the last window closed.

## Packed apps

`--pack` appends the bundle to a copy of the running executable, `[files][u64 length]["GPUIXQJS"]`.
At startup the trailer decides whether `argv[1]` is a bundle path or belongs to the app. macOS's
`codesign` refuses a Mach-O with data appended, so a signed `.app` instead keeps the bundle beside
the binary: `gpuix-app/` next to the executable, or `../Resources/gpuix-app/` in a bundle, with a
`gpuix-app.json` naming the `entry`. The host looks for the trailer first and the sidecar second.

## Building

The crate shares `packages/native`'s target directory (`.cargo/config.toml`) and its release
profile, so a build of either warms the other. Two things the profile does on purpose:

- `strip = "symbols"` for the final binary, with `strip = "none"` for build scripts and proc
  macros: Rust 1.97's default debuginfo stripping leaves a proc-macro dylib's string pool
  4-byte aligned and macOS 27's dyld refuses to load it (seen as "can't find crate for
  `rquickjs_macro`"). Rust 1.98 fixes it; the zed pin is 1.97.
- `build.rs` links the macOS binary with `-undefined dynamic_lookup`: `gpuix-native`'s napi
  bindings compile in and reference `napi_*` symbols only Node has, and nothing here calls them.
  Linux links without any flag. Windows has not been built yet.

### Linux from a Mac

`docker/Dockerfile` is a Debian image with Rust 1.97.1 and GPUI's system deps (the apt list from
`ci.yml`, plus Xvfb, Mesa's software Vulkan driver and ImageMagick). The header of the file has
the two commands; the target dir lives in a Docker volume so it never mixes with the macOS one.
A build from scratch takes about three and a half minutes on an 18-core OrbStack VM and gives a
32 MB arm64 binary (wgpu and the X11/Wayland clients are the difference from macOS). `tests/smoke.js`
passes there, and a bundle opens a mapped 820×560 window under `Xvfb` on lavapipe and mounts.
`captureScreenshot` is unavailable on Linux (no test-support build), and reading the frame back
out of Xvfb with `import`/`xwd` gave a blank image, so what the window paints there is unverified.

The changes this needed in `gpuix-native` are small and napi-neutral: `GpuixRenderer::with_event_callback`
(a plain-closure constructor the napi one delegates to), `prompt_for_paths_with` and the
`*_bytes` variants of the image and font uploads, and serde derives on the payload structs so a
host without napi can hand them over as JSON.

## npm packages

`npm/make.mjs <target> <version>` assembles `npm/gpuix-qjs-<platform>-<arch>/` from a built binary,
one package per platform with `os`/`cpu` set, which gpuix-svelte lists as optional dependencies
and finds by name. `.github/workflows/qjs.yml` builds and publishes them from a `gpuix-qjs@<version>`
tag.
