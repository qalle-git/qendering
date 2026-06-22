<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open } from "@tauri-apps/plugin-dialog";

  type DlcCount = { name: string; items: number };
  type ScanResult = { clothing_total: number; dlcs: DlcCount[]; objects: number };

  let inputDir = $state("");
  let outputDir = $state("");
  let mode = $state<"clothing" | "objects">("clothing");
  let format = $state<"webp" | "png" | "jpg">("webp");

  // Object camera / animation controls.
  let azimuth = $state(45);
  let elevation = $state(25);
  let animate = $state(false);

  let scan = $state<ScanResult | null>(null);
  let scanning = $state(false);

  let running = $state(false);
  let current = $state(0);
  let total = $state(0);
  let lastFile = $state("");
  let previewSrc = $state("");
  let processed = $state(0);
  let failed = $state(0);
  let log = $state<string[]>([]);

  // Gallery of rendered results.
  let outputs = $state<string[]>([]); // paths relative to <output>/textures
  let selectedIndex = $state(-1);
  let thumbs = $state<Record<string, string>>({}); // rel -> data URL (lazy)

  const canScan = $derived(!!inputDir && !scanning && !running);
  const canRender = $derived(!!inputDir && !!outputDir && !running);
  const pct = $derived(total > 0 ? Math.round((current / total) * 100) : 0);

  function addLog(msg: string) {
    log = [...log.slice(-400), msg];
  }

  async function pick(which: "in" | "out") {
    const dir = await open({ directory: true, multiple: false });
    if (typeof dir === "string") {
      if (which === "in") {
        inputDir = dir;
        scan = null;
        turntableFrames = [];
        previewMsg = "";
      } else {
        outputDir = dir;
        await refreshOutputs();
      }
    }
  }

  async function doScan() {
    if (!canScan) return;
    scanning = true;
    scan = null;
    try {
      scan = await invoke<ScanResult>("scan", { inputDir });
      addLog(
        `Scanned: ${scan.clothing_total} clothing across ${scan.dlcs.length} collection(s), ${scan.objects} object(s).`,
      );
    } catch (e) {
      addLog(`Scan failed: ${e}`);
    } finally {
      scanning = false;
    }
  }

  async function doRender() {
    if (!canRender) return;
    running = true;
    current = 0;
    total = 0;
    processed = 0;
    failed = 0;
    previewSrc = "";
    lastFile = "";
    selectedIndex = -1;
    addLog(`Starting ${mode} render (${animate && mode === "objects" ? "GIF" : format.toUpperCase()})…`);
    try {
      await invoke("start_render", {
        inputDir,
        outputDir,
        format,
        mode,
        azimuthDeg: azimuth,
        elevationDeg: elevation,
        animate: mode === "objects" ? animate : false,
      });
    } catch (e) {
      addLog(`Failed to start: ${e}`);
      running = false;
    }
  }

  // --- Gallery --------------------------------------------------------------

  async function refreshOutputs() {
    if (!outputDir) return;
    try {
      outputs = await invoke<string[]>("list_outputs", { outputDir });
    } catch (e) {
      outputs = [];
    }
  }

  async function loadThumb(rel: string): Promise<string> {
    const cached = thumbs[rel];
    if (cached) return cached;
    try {
      let url = await invoke<string>("read_image_data_url", {
        path: `${outputDir}/textures/${rel}`,
      });
      // Ensure GIFs animate even if the host labels them by a different mime.
      if (rel.toLowerCase().endsWith(".gif") && url.startsWith("data:image/")) {
        url = "data:image/gif;base64," + (url.split(",")[1] ?? "");
      }
      thumbs = { ...thumbs, [rel]: url };
      return url;
    } catch {
      return "";
    }
  }

  async function showOutput(i: number) {
    if (i < 0 || i >= outputs.length) return;
    selectedIndex = i;
    const url = await loadThumb(outputs[i]);
    if (url) previewSrc = url;
    queueMicrotask(() => {
      document
        .querySelector(`[data-idx="${i}"]`)
        ?.scrollIntoView({ block: "nearest", inline: "nearest" });
    });
  }

  // Lazy-load a thumbnail only when it scrolls near the viewport.
  function lazyload(node: HTMLElement, rel: string) {
    const io = new IntersectionObserver(
      (entries) => {
        for (const en of entries) {
          if (en.isIntersecting) {
            loadThumb(rel);
            io.unobserve(node);
          }
        }
      },
      { rootMargin: "300px" },
    );
    io.observe(node);
    return { destroy: () => io.disconnect() };
  }

  function onKey(e: KeyboardEvent) {
    if (!outputs.length) return;
    const tag = (e.target as HTMLElement | null)?.tagName;
    if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") return;
    let next: number;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      next = selectedIndex < 0 ? 0 : Math.min(outputs.length - 1, selectedIndex + 1);
    } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
      next = selectedIndex < 0 ? 0 : Math.max(0, selectedIndex - 1);
    } else {
      return;
    }
    e.preventDefault();
    showOutput(next);
  }

  // --- Turntable live preview (objects) ------------------------------------
  let turntableFrames = $state<string[]>([]); // data URLs, azimuth order
  let previewBuilding = $state(false);
  let previewMsg = $state("");

  function scrubTo(az: number) {
    const n = turntableFrames.length;
    if (!n) return;
    const idx = ((Math.round((az / 360) * n) % n) + n) % n;
    previewSrc = turntableFrames[idx];
  }

  async function buildTurntable() {
    if (mode !== "objects" || !inputDir || previewBuilding) return;
    previewBuilding = true;
    previewMsg = "Rendering turntable…";
    turntableFrames = [];
    try {
      const paths = await invoke<string[]>("preview_turntable", {
        inputDir,
        mode,
        elevationDeg: elevation,
        frames: 24,
      });
      const urls: string[] = [];
      for (const p of paths) {
        try {
          urls.push(await invoke<string>("read_image_data_url", { path: p }));
        } catch {
          /* skip a bad frame */
        }
      }
      turntableFrames = urls;
      if (urls.length) {
        previewMsg = "";
        selectedIndex = -1;
        scrubTo(azimuth);
      } else {
        previewMsg = "No frames produced.";
      }
    } catch (e) {
      previewMsg = `Preview failed: ${e}`;
    } finally {
      previewBuilding = false;
    }
  }

  // Live-scrub the turntable as the azimuth slider moves.
  $effect(() => {
    azimuth; // track for reactivity
    if (turntableFrames.length) scrubTo(azimuth);
  });

  // Wire backend events + keyboard once.
  $effect(() => {
    const unlisteners: Promise<UnlistenFn>[] = [];
    unlisteners.push(
      listen<{ total: number }>("start", (e) => {
        total = e.payload.total;
        current = 0;
      }),
    );
    unlisteners.push(
      listen<{ current: number; total: number; file: string; ok: boolean }>(
        "progress",
        async (e) => {
          current = e.payload.current;
          total = e.payload.total;
          lastFile = e.payload.file;
          if (e.payload.ok) {
            processed += 1;
            if (selectedIndex < 0) {
              try {
                const p = `${outputDir}/textures/${e.payload.file}`;
                previewSrc = await invoke<string>("read_image_data_url", { path: p });
              } catch {
                /* preview is best-effort */
              }
            }
          } else {
            failed += 1;
          }
        },
      ),
    );
    unlisteners.push(
      listen<{ processed: number; failed: number }>("done", async (e) => {
        processed = e.payload.processed;
        failed = e.payload.failed;
        running = false;
        addLog(`Done — ${e.payload.processed} rendered, ${e.payload.failed} failed.`);
        await refreshOutputs();
      }),
    );
    unlisteners.push(
      listen<{ message: string }>("error", (e) => {
        running = false;
        addLog(`Error: ${e.payload.message}`);
      }),
    );
    unlisteners.push(listen<string>("log", (e) => addLog(e.payload)));

    window.addEventListener("keydown", onKey);

    return () => {
      unlisteners.forEach((u) => u.then((fn) => fn()).catch(() => {}));
      window.removeEventListener("keydown", onKey);
    };
  });
</script>

<div class="app">
  <header class="topbar">
    <div class="brand">
      <span class="logo">Q</span>
      <div>
        <div class="title">Qendering</div>
        <div class="subtitle">GTA V clothing &amp; object preview renderer</div>
      </div>
    </div>
  </header>

  <div class="body">
    <aside class="panel">
      <section class="group">
        <label class="lbl">Input folder</label>
        <div class="pathrow">
          <span class="path" title={inputDir}>{inputDir || "Not selected"}</span>
          <button class="btn ghost" onclick={() => pick("in")}>Browse</button>
        </div>
      </section>

      <section class="group">
        <label class="lbl">Output folder</label>
        <div class="pathrow">
          <span class="path" title={outputDir}>{outputDir || "Not selected"}</span>
          <button class="btn ghost" onclick={() => pick("out")}>Browse</button>
        </div>
      </section>

      <section class="group">
        <label class="lbl">Asset type</label>
        <div class="segmented">
          <button class:active={mode === "clothing"} onclick={() => (mode = "clothing")}>
            Clothing
          </button>
          <button class:active={mode === "objects"} onclick={() => (mode = "objects")}>
            Objects
          </button>
        </div>
      </section>

      <section class="group">
        <label class="lbl">Output format</label>
        <select class="select" bind:value={format} disabled={mode === "objects" && animate}>
          <option value="webp">WebP</option>
          <option value="png">PNG</option>
          <option value="jpg">JPEG</option>
        </select>
      </section>

      {#if mode === "objects"}
        <section class="group">
          <label class="lbl">Camera angle</label>
          <div class="slider">
            <div class="sliderhead"><span>Azimuth</span><b>{azimuth}°</b></div>
            <input type="range" min="0" max="360" step="1" bind:value={azimuth} />
          </div>
          <div class="slider">
            <div class="sliderhead"><span>Elevation</span><b>{elevation}°</b></div>
            <input
              type="range"
              min="0"
              max="60"
              step="1"
              bind:value={elevation}
              onchange={() => {
                if (turntableFrames.length) buildTurntable();
              }}
            />
          </div>
          <button
            class="btn ghost"
            disabled={!inputDir || previewBuilding}
            onclick={buildTurntable}
          >
            {previewBuilding
              ? "Rendering preview…"
              : turntableFrames.length
                ? "Rebuild live preview"
                : "Live preview (rotate)"}
          </button>
          {#if previewBuilding}
            <div class="hint">Rendering the first object's turntable…</div>
          {:else if turntableFrames.length}
            <div class="hint">Drag <b>Azimuth</b> to rotate · {turntableFrames.length} frames.</div>
          {:else if previewMsg}
            <div class="hint">{previewMsg}</div>
          {/if}
          <label class="check">
            <input type="checkbox" bind:checked={animate} />
            <span>Animate — 2s spinning GIF</span>
          </label>
          {#if animate}
            <div class="hint">Outputs are saved as <b>.gif</b> (azimuth is swept 360°).</div>
          {/if}
        </section>
      {/if}

      <div class="actions">
        <button class="btn" disabled={!canScan} onclick={doScan}>
          {scanning ? "Scanning…" : "Scan"}
        </button>
        <button class="btn primary" disabled={!canRender} onclick={doRender}>
          {running ? "Rendering…" : "Render"}
        </button>
      </div>

      {#if scan}
        <section class="scancard">
          <div class="scanrow"><span>Clothing</span><b>{scan.clothing_total}</b></div>
          <div class="scanrow"><span>Objects</span><b>{scan.objects}</b></div>
          <div class="scanrow"><span>Collections</span><b>{scan.dlcs.length}</b></div>
          {#if scan.dlcs.length}
            <div class="dlcs">
              {#each scan.dlcs as d}
                <div class="dlc"><span title={d.name}>{d.name}</span><em>{d.items}</em></div>
              {/each}
            </div>
          {/if}
        </section>
      {/if}
    </aside>

    <main class="stage">
      <div class="preview">
        {#if previewSrc}
          <img src={previewSrc} alt="render preview" />
        {:else}
          <div class="placeholder">
            {running ? "Rendering…" : "Preview of the current render appears here"}
          </div>
        {/if}
      </div>

      <div class="progress">
        <div class="bar"><div class="fill" style="width:{pct}%"></div></div>
        <div class="meta">
          <span class="file" title={lastFile}>{lastFile || "—"}</span>
          <span class="count">
            {current}/{total}
            {#if processed || failed}· {processed} ok · {failed} failed{/if}
          </span>
        </div>
      </div>

      {#if outputs.length}
        <section class="gallery">
          <div class="galleryhead">
            <span>Rendered ({outputs.length})</span>
            <span class="hint">← → to cycle</span>
          </div>
          <div class="thumbs">
            {#each outputs as rel, i (rel)}
              <button
                class="thumb"
                class:active={i === selectedIndex}
                data-idx={i}
                title={rel}
                onclick={() => showOutput(i)}
                use:lazyload={rel}
              >
                {#if thumbs[rel]}
                  <img src={thumbs[rel]} alt={rel} loading="lazy" />
                {:else}
                  <span class="thumbph"></span>
                {/if}
              </button>
            {/each}
          </div>
        </section>
      {/if}

      <div class="log">
        {#each log as line}
          <div class="logline">{line}</div>
        {/each}
        {#if !log.length}
          <div class="logline muted">Logs will appear here.</div>
        {/if}
      </div>
    </main>
  </div>
</div>

<style>
  :global(html, body) {
    margin: 0;
    height: 100%;
  }
  :global(body) {
    font-family: "Segoe UI", Inter, system-ui, sans-serif;
    background: #0e0f13;
    color: #e6e8ee;
    -webkit-font-smoothing: antialiased;
  }

  .app {
    --bg: #0e0f13;
    --panel: #16181f;
    --panel2: #1c1f28;
    --border: #262a36;
    --muted: #8b90a0;
    --text: #e6e8ee;
    --accent: #6d5efc;
    --accent2: #8b7bff;
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .topbar {
    display: flex;
    align-items: center;
    padding: 14px 18px;
    border-bottom: 1px solid var(--border);
    background: linear-gradient(180deg, #15171e, #111319);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .logo {
    width: 34px;
    height: 34px;
    display: grid;
    place-items: center;
    border-radius: 9px;
    font-weight: 800;
    color: white;
    background: linear-gradient(135deg, var(--accent), var(--accent2));
    box-shadow: 0 4px 14px rgba(109, 94, 252, 0.4);
  }
  .title {
    font-weight: 700;
    font-size: 15px;
    letter-spacing: 0.2px;
  }
  .subtitle {
    font-size: 11.5px;
    color: var(--muted);
  }

  .body {
    flex: 1;
    display: grid;
    grid-template-columns: 320px 1fr;
    min-height: 0;
  }

  .panel {
    border-right: 1px solid var(--border);
    background: var(--panel);
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    overflow-y: auto;
  }
  .group {
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .lbl {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .pathrow {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .path {
    flex: 1;
    font-size: 12px;
    background: var(--panel2);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 10px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    color: #cfd3df;
  }

  .btn {
    border: 1px solid var(--border);
    background: var(--panel2);
    color: var(--text);
    border-radius: 8px;
    padding: 8px 14px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    transition: 0.15s;
  }
  .btn:hover:not(:disabled) {
    border-color: #3a4053;
    background: #232734;
  }
  .btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .btn.ghost {
    padding: 8px 12px;
    font-weight: 500;
  }
  .btn.primary {
    background: linear-gradient(135deg, var(--accent), var(--accent2));
    border-color: transparent;
    color: white;
    box-shadow: 0 4px 14px rgba(109, 94, 252, 0.35);
  }

  .segmented {
    display: flex;
    background: var(--panel2);
    border: 1px solid var(--border);
    border-radius: 9px;
    padding: 3px;
    gap: 3px;
  }
  .segmented button {
    flex: 1;
    border: none;
    background: transparent;
    color: var(--muted);
    padding: 7px;
    border-radius: 6px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .segmented button.active {
    background: var(--accent);
    color: white;
  }

  .select {
    background: var(--panel2);
    border: 1px solid var(--border);
    color: var(--text);
    border-radius: 8px;
    padding: 9px 10px;
    font-size: 13px;
  }
  .select:disabled {
    opacity: 0.5;
  }

  .slider {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .sliderhead {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
    color: var(--muted);
  }
  .sliderhead b {
    color: var(--text);
  }
  .slider input[type="range"] {
    width: 100%;
    accent-color: var(--accent);
  }
  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: #cfd3df;
    cursor: pointer;
  }
  .check input {
    accent-color: var(--accent);
  }
  .hint {
    font-size: 11.5px;
    color: var(--muted);
  }
  .hint b {
    color: #cfd3df;
  }

  .actions {
    display: flex;
    gap: 8px;
  }
  .actions .btn {
    flex: 1;
  }

  .scancard {
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel2);
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .scanrow {
    display: flex;
    justify-content: space-between;
    font-size: 13px;
    color: #cfd3df;
  }
  .scanrow b {
    color: var(--text);
  }
  .dlcs {
    margin-top: 6px;
    border-top: 1px solid var(--border);
    padding-top: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: 160px;
    overflow-y: auto;
  }
  .dlc {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    font-size: 12px;
    color: var(--muted);
  }
  .dlc span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dlc em {
    font-style: normal;
    color: #cfd3df;
  }

  .stage {
    display: flex;
    flex-direction: column;
    min-height: 0;
    padding: 16px;
    gap: 14px;
  }
  .preview {
    flex: 1;
    display: grid;
    place-items: center;
    border: 1px solid var(--border);
    border-radius: 12px;
    background:
      repeating-conic-gradient(#1a1d25 0% 25%, #15171e 0% 50%) 50% / 22px 22px;
    min-height: 0;
    overflow: hidden;
  }
  .preview img {
    max-width: 92%;
    max-height: 92%;
    object-fit: contain;
    filter: drop-shadow(0 10px 30px rgba(0, 0, 0, 0.5));
  }
  .placeholder {
    color: var(--muted);
    font-size: 13px;
  }

  .progress {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bar {
    height: 8px;
    background: var(--panel2);
    border: 1px solid var(--border);
    border-radius: 999px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: linear-gradient(90deg, var(--accent), var(--accent2));
    transition: width 0.2s;
  }
  .meta {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
    color: var(--muted);
    gap: 12px;
  }
  .meta .file {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta .count {
    flex-shrink: 0;
  }

  .gallery {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .galleryhead {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .thumbs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(64px, 1fr));
    gap: 6px;
    max-height: 168px;
    overflow-y: auto;
    padding: 6px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: #0b0c10;
  }
  .thumb {
    aspect-ratio: 1;
    padding: 0;
    border: 2px solid transparent;
    border-radius: 8px;
    overflow: hidden;
    cursor: pointer;
    background:
      repeating-conic-gradient(#1a1d25 0% 25%, #15171e 0% 50%) 50% / 10px 10px;
  }
  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
  }
  .thumb.active {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px rgba(109, 94, 252, 0.35);
  }
  .thumbph {
    display: block;
    width: 100%;
    height: 100%;
  }

  .log {
    height: 130px;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: #0b0c10;
    padding: 10px 12px;
    font-family: "Cascadia Code", "Consolas", monospace;
    font-size: 11.5px;
    line-height: 1.55;
  }
  .logline {
    color: #b9becb;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .logline.muted {
    color: var(--muted);
  }
</style>
