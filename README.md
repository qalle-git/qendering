# Qendering

A desktop app for batch-rendering GTA V clothing (`.ydd`) and world objects (`.ydr`)
into clean preview images for FiveM shops and catalogs.

Rewrite of an earlier Python tool as a **Tauri 2** app: a **Rust** core + a Svelte
UI, with the actual 3D rendering driven through **Blender + Sollumz**.

## Status

🚧 Under active development. Foundation: Tauri + Svelte scaffold and the CI
release pipeline. Core (RSC7/YTD/DDS parsing, render orchestration) and UI are
being ported next.

## Architecture

- **`src-tauri/`** — Tauri app (Rust). Hosts the UI and orchestrates renders.
- **`crates/qendering-core/`** — pure-Rust core: RSC7 decode, YTD parsing,
  DDS decode, image resize + WebP/PNG/JPEG encode, filename parsing, and
  clothing/object discovery.
- **`python/`** — the in-Blender render script (Python — it runs inside
  Blender's embedded interpreter via Sollumz, so it stays Python).
- **`src/`** — Svelte (SvelteKit, static) front-end.
- **`.github/workflows/release.yml`** — builds the Windows `.exe` and attaches
  it to a GitHub Release on each `v*` tag.

## Runtime requirements

Rendering runs through Blender, so the machine running Qendering needs:

- **Blender 4.x** with the **Sollumz** add-on (and **PyMateria** for binary
  `.ydd`/`.ydr` import).

Qendering detects Blender on first run and guides setup.

## Development

```bash
npm install
npm run tauri dev      # run the app
npm run tauri build    # build a local .exe
```

## Releases

Push a tag to build and publish the installer:

```bash
git tag v0.1.0 && git push origin v0.1.0
```
