<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open } from "@tauri-apps/plugin-dialog";

  type DlcCount = { name: string; items: number };
  type ScanResult = { clothing_total: number; dlcs: DlcCount[]; objects: number };

  let inputDir = $state("");
  let outputDir = $state("");
  let mode = $state<"clothing" | "objects" | "weapons">("clothing");
  let format = $state<"webp" | "png" | "jpg">("webp");

  // Object camera / animation controls.
  let azimuth = $state(45);
  let elevation = $state(25);
  let animate = $state(false);
  // Per-pack batch: render each top-level pack in its own isolated worker pool
  // and output subfolder/manifest.
  let batch = $state(false);
  // Clothing: 3D-render the .ydd drawable in Blender instead of the fast flat
  // texture extraction.
  let clothing3d = $state(false);
  // Skip a Blender item that hangs longer than this (seconds).
  let timeoutSecs = $state(30);

  // Weapons mode: one weapon + chosen attachment models, rendered as one still.
  let weaponPath = $state("");
  let weaponAttachments = $state<string[]>([]); // candidate attachment paths
  let selectedAttachments = $state<string[]>([]); // chosen attachment paths
  let weaponBuilding = $state(false);
  let weaponError = $state("");

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

  // Per-render output subfolder (dated/labeled).
  let useSubfolder = $state(false);
  let label = $state("");
  let stopping = $state(false);

  // Gallery of rendered results.
  let outputs = $state<string[]>([]); // paths relative to <dir>/textures
  let selectedIndex = $state(-1); // index into filteredOutputs
  let thumbs = $state<Record<string, string>>({}); // rel -> data URL (lazy)
  let filter = $state("");
  let currentOutputDir = $state(""); // folder the gallery reads from
  const filteredOutputs = $derived(
    filter.trim()
      ? outputs.filter((r) => r.toLowerCase().includes(filter.trim().toLowerCase()))
      : outputs,
  );

  const canScan = $derived(!!inputDir && !scanning && !running);
  const canRender = $derived(
    !!outputDir && !running && (mode === "weapons" ? !!weaponPath : !!inputDir),
  );
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
        currentOutputDir = dir;
        await refreshOutputs();
      }
    }
  }

  function baseName(p: string): string {
    return p.split(/[\\/]/).pop() ?? p;
  }

  async function pickWeapon() {
    const sel = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Weapon", extensions: ["ydr", "yft"] }],
    });
    if (typeof sel === "string") {
      weaponPath = sel;
      previewSrc = "";
      weaponError = "";
      selectedAttachments = [];
      try {
        weaponAttachments = await invoke<string[]>("list_weapon_attachments", {
          weaponPath: sel,
        });
      } catch (e) {
        weaponError = `Could not list attachments: ${e}`;
        weaponAttachments = [];
      }
    }
  }

  function toggleAttachment(p: string) {
    selectedAttachments = selectedAttachments.includes(p)
      ? selectedAttachments.filter((x) => x !== p)
      : [...selectedAttachments, p];
  }

  async function buildWeapon() {
    if (!weaponPath) {
      weaponError = "Pick a weapon file first.";
      return;
    }
    if (!outputDir) {
      weaponError = "Pick an output folder first.";
      return;
    }
    weaponError = "";
    weaponBuilding = true;
    previewSrc = "";
    addLog(`Rendering weapon ${baseName(weaponPath)}…`);
    try {
      const imgPath = await invoke<string>("render_weapon", {
        weaponPath,
        attachmentPaths: selectedAttachments,
        format,
        azimuthDeg: azimuth,
        elevationDeg: elevation,
        outputDir,
        timeoutSecs: Math.max(10, Math.round(timeoutSecs) || 30),
      });
      previewSrc = await invoke<string>("read_image_data_url", { path: imgPath });
      currentOutputDir = outputDir;
      await refreshOutputs();
    } catch (e) {
      weaponError = String(e);
      addLog(`Weapon render failed: ${e}`);
    } finally {
      weaponBuilding = false;
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

  function sanitizeLabel(s: string): string {
    return s.trim().replace(/\s+/g, "_").replace(/[^A-Za-z0-9_-]/g, "");
  }

  function makeTimestamp(): string {
    const d = new Date();
    const p = (n: number) => String(n).padStart(2, "0");
    return (
      `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}` +
      `_${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}`
    );
  }

  function computeSubfolder(): string {
    if (!useSubfolder) return "";
    const lab = sanitizeLabel(label);
    return lab ? `${makeTimestamp()}_${lab}` : makeTimestamp();
  }

  async function doRender() {
    if (mode === "weapons") {
      await buildWeapon();
      return;
    }
    if (!canRender) return;
    const subfolder = computeSubfolder();
    currentOutputDir = subfolder ? `${outputDir}/${subfolder}` : outputDir;
    running = true;
    stopping = false;
    current = 0;
    total = 0;
    processed = 0;
    failed = 0;
    previewSrc = "";
    lastFile = "";
    selectedIndex = -1;
    filter = "";
    outputs = [];
    addLog(
      `Starting ${mode} render (${animate && mode === "objects" ? "GIF" : format.toUpperCase()})` +
        (subfolder ? ` → ${subfolder}/` : "") +
        "…",
    );
    try {
      await invoke("start_render", {
        inputDir,
        outputDir,
        format,
        mode,
        azimuthDeg: azimuth,
        elevationDeg: elevation,
        animate: mode === "objects" ? animate : false,
        subfolder,
        batch: mode === "objects" ? batch : false,
        clothing3d: mode === "clothing" ? clothing3d : false,
        timeoutSecs: Math.max(3, Math.round(timeoutSecs) || 30),
      });
    } catch (e) {
      addLog(`Failed to start: ${e}`);
      running = false;
    }
  }

  async function stopRender() {
    if (!running || stopping) return;
    stopping = true;
    addLog("Stopping after the current item…");
    try {
      await invoke("cancel_render");
    } catch (e) {
      addLog(`Stop failed: ${e}`);
      stopping = false;
    }
  }

  // --- Gallery --------------------------------------------------------------

  async function refreshOutputs() {
    const dir = currentOutputDir || outputDir;
    if (!dir) return;
    try {
      outputs = await invoke<string[]>("list_outputs", { outputDir: dir });
    } catch (e) {
      outputs = [];
    }
  }

  /// Clear the gallery view (UI only — does not delete any files on disk).
  function clearGallery() {
    outputs = [];
    thumbs = {};
    selectedIndex = -1;
    previewSrc = "";
    filter = "";
  }

  async function loadThumb(rel: string): Promise<string> {
    const cached = thumbs[rel];
    if (cached) return cached;
    try {
      let url = await invoke<string>("read_image_data_url", {
        path: `${currentOutputDir || outputDir}/textures/${rel}`,
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
    const list = filteredOutputs;
    if (i < 0 || i >= list.length) return;
    selectedIndex = i;
    const url = await loadThumb(list[i]);
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
    const list = filteredOutputs;
    if (!list.length) return;
    const tag = (e.target as HTMLElement | null)?.tagName;
    if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") return;
    let next: number;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      next = selectedIndex < 0 ? 0 : Math.min(list.length - 1, selectedIndex + 1);
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
  let previewFrames = $state(24); // turntable resolution (more = smoother)
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
        frames: previewFrames,
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
                const p = `${currentOutputDir || outputDir}/textures/${e.payload.file}`;
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
        stopping = false;
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
          <button class:active={mode === "weapons"} onclick={() => (mode = "weapons")}>
            Weapons
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

      {#if mode === "clothing"}
        <section class="group">
          <label class="check">
            <input type="checkbox" bind:checked={clothing3d} />
            <span>3D render (Blender)</span>
          </label>
          <div class="hint">
            {clothing3d
              ? "Imports each .ydd drawable in Blender for a 3D preview (slower; needs Blender + Sollumz)."
              : "Fast flat texture extraction straight from the .ytd (no Blender)."}
          </div>
        </section>
      {/if}

      <section class="group">
        <label class="check">
          <input type="checkbox" bind:checked={useSubfolder} />
          <span>Save into a dated subfolder</span>
        </label>
        {#if useSubfolder}
          <input class="textin" type="text" placeholder="Label (optional)" bind:value={label} />
          <div class="hint">
            Folder like <b>2026-06-22_19-30-05{label.trim() ? "_" + sanitizeLabel(label) : ""}</b>/
          </div>
        {/if}
      </section>

      {#if mode === "weapons"}
        <section class="group">
          <label class="lbl">Weapon</label>
          <button class="btn ghost" onclick={pickWeapon}>
            {weaponPath ? baseName(weaponPath) : "Pick weapon (.ydr / .yft)"}
          </button>
          {#if weaponAttachments.length}
            <label class="lbl" style="margin-top:10px;">Attachments</label>
            <div style="max-height:180px;overflow-y:auto;display:flex;flex-direction:column;gap:2px;">
              {#each weaponAttachments as att}
                <label class="check">
                  <input
                    type="checkbox"
                    checked={selectedAttachments.includes(att)}
                    onchange={() => toggleAttachment(att)}
                  />
                  <span>{baseName(att)}</span>
                </label>
              {/each}
            </div>
          {:else if weaponPath}
            <div class="hint">No sibling .ydr/.yft attachments found in that folder.</div>
          {/if}
          <div class="hint">Set an output folder, then click <b>Render</b> to preview.</div>
          {#if weaponError}
            <div class="hint" style="color:#ff6b6b;">{weaponError}</div>
          {/if}
        </section>
      {/if}

      {#if mode === "objects" || (mode === "clothing" && clothing3d) || mode === "weapons"}
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
          {#if mode === "clothing"}
            <div class="hint">Azimuth <b>0°</b> faces the piece head-on; raise it for a 3/4 view.</div>
          {/if}
          <div class="slider">
            <div class="sliderhead"><span>Skip item after</span><b>{timeoutSecs}s</b></div>
            <input type="range" min="5" max="120" step="5" bind:value={timeoutSecs} />
          </div>
          <div class="hint">A Blender render that hangs longer than this is skipped and marked failed.</div>
        </section>
      {/if}

      {#if mode === "objects"}
        <section class="group">
          <div class="slider">
            <div class="sliderhead"><span>Preview frames</span><b>{previewFrames}</b></div>
            <input
              type="range"
              min="8"
              max="64"
              step="4"
              bind:value={previewFrames}
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
          <label class="check">
            <input type="checkbox" bind:checked={batch} />
            <span>Per-pack batch</span>
          </label>
          {#if batch}
            <div class="hint">
              Each top-level pack folder renders in its own worker pool and gets its own
              subfolder + <b>manifest.json</b>. A crash in one pack won't stop the others.
            </div>
          {/if}
        </section>
      {/if}

      <div class="actions">
        <button class="btn" disabled={!canScan} onclick={doScan}>
          {scanning ? "Scanning…" : "Scan"}
        </button>
        {#if running}
          <button class="btn stop" disabled={stopping} onclick={stopRender}>
            {stopping ? "Stopping…" : "Stop"}
          </button>
        {:else}
          <button class="btn primary" disabled={!canRender || weaponBuilding} onclick={doRender}>
            {weaponBuilding ? "Rendering…" : "Render"}
          </button>
        {/if}
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
            {running || weaponBuilding ? "Rendering…" : "Preview of the current render appears here"}
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
            <span class="gtitle">
              {filteredOutputs.length === outputs.length
                ? `${outputs.length} results`
                : `${filteredOutputs.length} / ${outputs.length} results`}
            </span>
            <input
              class="filter"
              type="text"
              placeholder="Filter by name…"
              bind:value={filter}
              oninput={() => (selectedIndex = -1)}
            />
            {#if outputs.length}
              <button
                class="btn ghost"
                onclick={clearGallery}
                title="Clear the gallery view (does not delete files)"
              >
                Clear
              </button>
            {/if}
            <span class="hint">← → cycle</span>
          </div>
          {#if filteredOutputs.length}
            <div class="thumbs">
              {#each filteredOutputs as rel, i (rel)}
                <div class="card" class:active={i === selectedIndex}>
                  <button
                    class="thumb"
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
                  <div class="cardlabel" title={rel}>{rel.split("/").pop()}</div>
                </div>
              {/each}
            </div>
          {:else}
            <div class="hint nomatch">No results match “{filter}”.</div>
          {/if}
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
  .btn.stop {
    background: #c0392b;
    border-color: transparent;
    color: white;
  }
  .btn.stop:hover:not(:disabled) {
    background: #d0463a;
  }
  .textin {
    width: 100%;
    box-sizing: border-box;
    background: var(--panel2);
    border: 1px solid var(--border);
    border-radius: 8px;
    color: var(--text);
    padding: 8px 10px;
    font-size: 13px;
    margin-top: 6px;
  }
  .textin:focus {
    outline: none;
    border-color: var(--accent);
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
    gap: 8px;
    padding-top: 12px;
    border-top: 1px solid var(--border);
  }
  .galleryhead {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .galleryhead .gtitle {
    font-weight: 700;
    color: var(--text);
    white-space: nowrap;
  }
  .filter {
    flex: 1;
    min-width: 0;
    background: var(--panel2);
    border: 1px solid var(--border);
    border-radius: 7px;
    color: var(--text);
    padding: 5px 9px;
    font-size: 12px;
    text-transform: none;
    letter-spacing: 0;
  }
  .filter:focus {
    outline: none;
    border-color: var(--accent);
  }
  .nomatch {
    padding: 14px;
    text-align: center;
  }
  .thumbs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(96px, 1fr));
    gap: 10px;
    max-height: 300px;
    overflow-y: auto;
    padding: 10px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: #0b0c10;
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 6px;
    border: 1px solid var(--border);
    border-radius: 9px;
    background: var(--panel);
    transition: 0.12s;
  }
  .card.active {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px rgba(109, 94, 252, 0.3);
  }
  .thumb {
    aspect-ratio: 1;
    padding: 0;
    border: none;
    border-radius: 6px;
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
  .thumbph {
    display: block;
    width: 100%;
    height: 100%;
  }
  .cardlabel {
    font-size: 10px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    text-align: center;
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
