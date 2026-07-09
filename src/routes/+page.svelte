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

  const modeBlurb = $derived(
    mode === "clothing"
      ? "Preview clothing textures pulled straight from each .ytd."
      : mode === "objects"
        ? "3/4 product shots of standalone .ydr world objects."
        : "Assemble a weapon with its attachments as one shot.",
  );
</script>

<div class="app">
  <header class="topbar">
    <div class="brand">
      <span class="logo">Q</span>
      <div class="brandtext">
        <div class="title">Qendering</div>
        <div class="subtitle">GTA V asset preview renderer</div>
      </div>
    </div>
    <div class="status" class:busy={running || weaponBuilding}>
      <span class="dot"></span>
      {running || weaponBuilding ? "Rendering" : "Ready"}
    </div>
  </header>

  <div class="body">
    <aside class="panel">
      <div class="scroll">
        <!-- Step 1: folders -->
        <section class="group">
          <h2 class="steplabel"><span class="stepnum">1</span> Folders</h2>
          <div class="field">
            <span class="fieldlabel">Input</span>
            <button class="pathbtn" onclick={() => pick("in")} title={inputDir}>
              <span class="path" class:empty={!inputDir}>
                {inputDir ? baseName(inputDir) : "Choose input folder"}
              </span>
              <span class="pathaction">Browse</span>
            </button>
          </div>
          <div class="field">
            <span class="fieldlabel">Output</span>
            <button class="pathbtn" onclick={() => pick("out")} title={outputDir}>
              <span class="path" class:empty={!outputDir}>
                {outputDir ? baseName(outputDir) : "Choose output folder"}
              </span>
              <span class="pathaction">Browse</span>
            </button>
          </div>
        </section>

        <!-- Step 2: asset type -->
        <section class="group">
          <h2 class="steplabel"><span class="stepnum">2</span> Asset type</h2>
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
          <p class="hint">{modeBlurb}</p>
        </section>

        <!-- Step 3 (weapons): weapon + attachments -->
        {#if mode === "weapons"}
          <section class="group">
            <h2 class="steplabel"><span class="stepnum">3</span> Weapon &amp; attachments</h2>
            <button class="pathbtn" onclick={pickWeapon} title={weaponPath}>
              <span class="path" class:empty={!weaponPath}>
                {weaponPath ? baseName(weaponPath) : "Pick weapon (.ydr / .yft)"}
              </span>
              <span class="pathaction">Browse</span>
            </button>
            {#if weaponAttachments.length}
              <div class="attlist">
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
              <p class="hint">No .ydr/.yft attachments sit beside that weapon.</p>
            {/if}
            {#if weaponError}<p class="hint err">{weaponError}</p>{/if}
          </section>
        {/if}

        <!-- Options -->
        <section class="group">
          <h2 class="steplabel">
            <span class="stepnum">{mode === "weapons" ? 4 : 3}</span> Options
          </h2>

          <div class="field">
            <span class="fieldlabel">Format</span>
            <select class="select" bind:value={format} disabled={mode === "objects" && animate}>
              <option value="webp">WebP</option>
              <option value="png">PNG</option>
              <option value="jpg">JPEG</option>
            </select>
            {#if mode === "objects" && animate}
              <p class="hint">Animated runs are saved as .gif.</p>
            {/if}
          </div>

          {#if mode === "clothing"}
            <label class="check">
              <input type="checkbox" bind:checked={clothing3d} />
              <span>3D render in Blender</span>
            </label>
            <p class="hint">
              {clothing3d
                ? "Imports each .ydd drawable for a true 3D preview (needs Blender + Sollumz)."
                : "Fast flat texture extraction from the .ytd — no Blender needed."}
            </p>
          {/if}

          {#if mode === "objects" || (mode === "clothing" && clothing3d) || mode === "weapons"}
            <div class="field">
              <span class="fieldlabel">Camera angle</span>
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
                <p class="hint">Azimuth 0° faces the piece head-on; raise it for a 3/4 view.</p>
              {/if}
            </div>
          {/if}

          {#if mode === "objects"}
            <button
              class="btn ghost full"
              disabled={!inputDir || previewBuilding}
              onclick={buildTurntable}
            >
              {previewBuilding
                ? "Rendering preview…"
                : turntableFrames.length
                  ? "Rebuild live preview"
                  : "Live rotate preview"}
            </button>
            {#if turntableFrames.length && !previewBuilding}
              <p class="hint">Drag azimuth to rotate · {turntableFrames.length} frames.</p>
            {:else if previewMsg}
              <p class="hint">{previewMsg}</p>
            {/if}
          {/if}
        </section>

        <!-- Advanced (collapsed by default) -->
        <details class="advanced">
          <summary>Advanced</summary>
          <div class="advbody">
            <div class="slider">
              <div class="sliderhead"><span>Skip item after</span><b>{timeoutSecs}s</b></div>
              <input type="range" min="5" max="120" step="5" bind:value={timeoutSecs} />
            </div>
            <p class="hint">A Blender render that hangs longer than this is skipped.</p>

            <label class="check">
              <input type="checkbox" bind:checked={useSubfolder} />
              <span>Save into a dated subfolder</span>
            </label>
            {#if useSubfolder}
              <input class="textin" type="text" placeholder="Label (optional)" bind:value={label} />
              <p class="hint">
                Folder: 2026-06-22_19-30-05{label.trim() ? "_" + sanitizeLabel(label) : ""}/
              </p>
            {/if}

            {#if mode === "objects"}
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
              <label class="check">
                <input type="checkbox" bind:checked={animate} />
                <span>Animate — 2s spinning GIF</span>
              </label>
              <label class="check">
                <input type="checkbox" bind:checked={batch} />
                <span>Per-pack batch</span>
              </label>
              {#if batch}
                <p class="hint">
                  Each pack renders in its own worker pool with its own manifest, so one bad pack
                  cannot stop the rest.
                </p>
              {/if}
            {/if}
          </div>
        </details>

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
      </div>

      <!-- Sticky actions -->
      <div class="actions">
        <button class="btn secondary" disabled={!canScan} onclick={doScan}>
          {scanning ? "Scanning…" : "Scan"}
        </button>
        {#if running}
          <button class="btn stop grow" disabled={stopping} onclick={stopRender}>
            {stopping ? "Stopping…" : "Stop"}
          </button>
        {:else}
          <button class="btn primary grow" disabled={!canRender || weaponBuilding} onclick={doRender}>
            {weaponBuilding ? "Rendering…" : "Render"}
          </button>
        {/if}
      </div>
    </aside>

    <main class="stage">
      <div class="preview">
        {#if previewSrc}
          <img src={previewSrc} alt="Render preview" />
        {:else}
          <div class="placeholder">
            <div class="ph-icon" aria-hidden="true">⬚</div>
            <div class="ph-title">{running || weaponBuilding ? "Rendering…" : "No preview yet"}</div>
            <div class="ph-sub">
              {running || weaponBuilding
                ? "Your render will appear here."
                : "Pick folders and hit Render to see a preview."}
            </div>
          </div>
        {/if}
      </div>

      <div class="progress" class:on={running || total > 0}>
        <div class="bar"><div class="fill" style="width:{pct}%"></div></div>
        <div class="meta">
          <span class="file" title={lastFile}>{lastFile || "Idle"}</span>
          <span class="count">
            {#if total > 0}<span class="frac">{current}/{total}</span>{/if}
            {#if processed}<span class="ok">{processed} ok</span>{/if}
            {#if failed}<span class="fail">{failed} failed</span>{/if}
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
            <button
              class="btn ghost sm"
              onclick={clearGallery}
              title="Clear the gallery view (does not delete files)"
            >
              Clear
            </button>
            <span class="hint kbd">← → cycle</span>
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

      <details class="logwrap" open>
        <summary>Activity{#if log.length} · {log.length}{/if}</summary>
        <div class="log">
          {#each log as line}
            <div class="logline">{line}</div>
          {/each}
          {#if !log.length}
            <div class="logline muted">Activity will appear here.</div>
          {/if}
        </div>
      </details>
    </main>
  </div>
</div>

<style>
  :global(html, body) {
    margin: 0;
    height: 100%;
  }
  :global(body) {
    font-family: system-ui, "Segoe UI", Roboto, sans-serif;
    background: #0d0e12;
    color: #e7e9f0;
    -webkit-font-smoothing: antialiased;
  }

  .app {
    --bg: #0d0e12;
    --panel: #14151b;
    --elev: #1a1c24;
    --elev2: #21232d;
    --border: #272a34;
    --border-soft: #1e2029;
    --text: #e7e9f0;
    --text-dim: #b3b8c6;
    --muted: #767c8c;
    --accent: #4c7ef3;
    --accent-press: #3f6ede;
    --accent-soft: rgba(76, 126, 243, 0.16);
    --danger: #e5544b;
    --ok: #4ec98a;
    --r: 10px;
    --r-sm: 7px;
    --s1: 4px;
    --s2: 8px;
    --s3: 12px;
    --s4: 16px;
    --s5: 24px;

    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  button {
    font-family: inherit;
  }

  /* Focus rings, consistent everywhere. */
  :where(button, input, select, summary, .pathbtn):focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  /* --- Top bar --- */
  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--s3) var(--s4);
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--s3);
  }
  .logo {
    width: 32px;
    height: 32px;
    display: grid;
    place-items: center;
    border-radius: var(--r-sm);
    font-weight: 700;
    font-size: 17px;
    color: #fff;
    background: var(--accent);
  }
  .title {
    font-weight: 650;
    font-size: 14px;
    letter-spacing: -0.01em;
  }
  .subtitle {
    font-size: 11.5px;
    color: var(--muted);
  }
  .status {
    display: flex;
    align-items: center;
    gap: var(--s2);
    font-size: 12px;
    color: var(--muted);
  }
  .status .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--muted);
  }
  .status.busy {
    color: var(--text-dim);
  }
  .status.busy .dot {
    background: var(--accent);
    box-shadow: 0 0 0 0 var(--accent-soft);
    animation: pulse 1.4s ease-out infinite;
  }
  @keyframes pulse {
    0% {
      box-shadow: 0 0 0 0 var(--accent-soft);
    }
    100% {
      box-shadow: 0 0 0 6px rgba(76, 126, 243, 0);
    }
  }

  /* --- Layout --- */
  .body {
    flex: 1;
    display: grid;
    grid-template-columns: 328px 1fr;
    min-height: 0;
  }

  .panel {
    border-right: 1px solid var(--border);
    background: var(--panel);
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .scroll {
    flex: 1;
    overflow-y: auto;
    padding: var(--s5) var(--s4) var(--s4);
    display: flex;
    flex-direction: column;
    gap: var(--s5);
  }

  .group {
    display: flex;
    flex-direction: column;
    gap: var(--s3);
  }
  .steplabel {
    display: flex;
    align-items: center;
    gap: var(--s2);
    margin: 0;
    font-size: 12.5px;
    font-weight: 600;
    color: var(--text);
    letter-spacing: -0.005em;
  }
  .stepnum {
    width: 18px;
    height: 18px;
    display: grid;
    place-items: center;
    border-radius: 50%;
    background: var(--elev2);
    color: var(--text-dim);
    font-size: 11px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--s2);
  }
  .fieldlabel {
    font-size: 11px;
    color: var(--muted);
  }

  /* Clickable folder / file rows. */
  .pathbtn {
    display: flex;
    align-items: center;
    gap: var(--s2);
    width: 100%;
    text-align: left;
    background: var(--elev);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--s2) var(--s2) var(--s2) var(--s3);
    cursor: pointer;
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .pathbtn:hover {
    border-color: #363b48;
    background: var(--elev2);
  }
  .pathbtn:active {
    transform: translateY(1px);
  }
  .path {
    flex: 1;
    font-size: 12.5px;
    color: var(--text);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .path.empty {
    color: var(--muted);
  }
  .pathaction {
    flex-shrink: 0;
    font-size: 11.5px;
    font-weight: 600;
    color: var(--text-dim);
    background: var(--elev2);
    border-radius: 5px;
    padding: 4px 9px;
  }

  /* --- Buttons --- */
  .btn {
    border: 1px solid var(--border);
    background: var(--elev);
    color: var(--text);
    border-radius: var(--r-sm);
    padding: var(--s2) var(--s4);
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    transition:
      background 0.15s,
      border-color 0.15s,
      transform 0.05s;
  }
  .btn:hover:not(:disabled) {
    border-color: #363b48;
    background: var(--elev2);
  }
  .btn:active:not(:disabled) {
    transform: translateY(1px);
  }
  .btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .btn.ghost {
    background: transparent;
    font-weight: 500;
  }
  .btn.ghost:hover:not(:disabled) {
    background: var(--elev);
  }
  .btn.full {
    width: 100%;
  }
  .btn.sm {
    padding: 5px 10px;
    font-size: 12px;
  }
  .btn.primary {
    background: var(--accent);
    border-color: transparent;
    color: #fff;
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-press);
    border-color: transparent;
  }
  .btn.secondary {
    background: var(--elev);
  }
  .btn.stop {
    background: var(--danger);
    border-color: transparent;
    color: #fff;
  }
  .btn.stop:hover:not(:disabled) {
    background: #ef6157;
    border-color: transparent;
  }
  .btn.grow {
    flex: 1;
  }

  /* --- Segmented control --- */
  .segmented {
    display: flex;
    background: var(--elev);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 3px;
    gap: 3px;
  }
  .segmented button {
    flex: 1;
    border: none;
    background: transparent;
    color: var(--muted);
    padding: 7px;
    border-radius: 5px;
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
    transition:
      color 0.15s,
      background 0.15s;
  }
  .segmented button:hover:not(.active) {
    color: var(--text-dim);
    background: var(--elev2);
  }
  .segmented button.active {
    background: var(--accent);
    color: #fff;
  }

  /* --- Inputs --- */
  .select,
  .textin {
    width: 100%;
    box-sizing: border-box;
    background: var(--elev);
    border: 1px solid var(--border);
    color: var(--text);
    border-radius: var(--r-sm);
    padding: var(--s2) var(--s3);
    font-size: 13px;
  }
  .select:disabled {
    opacity: 0.5;
  }
  .select:focus,
  .textin:focus {
    outline: none;
    border-color: var(--accent);
  }

  .slider {
    display: flex;
    flex-direction: column;
    gap: var(--s1);
  }
  .sliderhead {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
    color: var(--muted);
  }
  .sliderhead b {
    color: var(--text);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .slider input[type="range"] {
    width: 100%;
    accent-color: var(--accent);
  }

  .check {
    display: flex;
    align-items: center;
    gap: var(--s2);
    font-size: 13px;
    color: var(--text-dim);
    cursor: pointer;
  }
  .check input {
    accent-color: var(--accent);
    width: 15px;
    height: 15px;
  }

  .hint {
    margin: 0;
    font-size: 11.5px;
    line-height: 1.5;
    color: var(--muted);
  }
  .hint.err {
    color: var(--danger);
  }
  .hint.kbd {
    flex-shrink: 0;
  }

  .attlist {
    display: flex;
    flex-direction: column;
    gap: var(--s1);
    max-height: 190px;
    overflow-y: auto;
    background: var(--elev);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--s2) var(--s3);
  }

  /* --- Advanced disclosure --- */
  .advanced {
    border-top: 1px solid var(--border-soft);
    padding-top: var(--s4);
  }
  .advanced > summary {
    list-style: none;
    cursor: pointer;
    font-size: 12.5px;
    font-weight: 600;
    color: var(--text-dim);
    display: flex;
    align-items: center;
    gap: var(--s2);
    user-select: none;
  }
  .advanced > summary::-webkit-details-marker {
    display: none;
  }
  .advanced > summary::before {
    content: "›";
    display: inline-block;
    font-size: 15px;
    color: var(--muted);
    transition: transform 0.15s;
  }
  .advanced[open] > summary::before {
    transform: rotate(90deg);
  }
  .advbody {
    display: flex;
    flex-direction: column;
    gap: var(--s3);
    padding-top: var(--s4);
  }

  /* --- Sticky actions --- */
  .actions {
    display: flex;
    gap: var(--s2);
    padding: var(--s3) var(--s4);
    border-top: 1px solid var(--border);
    background: var(--panel);
  }

  /* --- Scan card --- */
  .scancard {
    border: 1px solid var(--border);
    border-radius: var(--r);
    background: var(--elev);
    padding: var(--s3);
    display: flex;
    flex-direction: column;
    gap: var(--s2);
  }
  .scanrow {
    display: flex;
    justify-content: space-between;
    font-size: 13px;
    color: var(--text-dim);
  }
  .scanrow b {
    color: var(--text);
    font-variant-numeric: tabular-nums;
  }
  .dlcs {
    margin-top: var(--s1);
    border-top: 1px solid var(--border);
    padding-top: var(--s2);
    display: flex;
    flex-direction: column;
    gap: var(--s1);
    max-height: 160px;
    overflow-y: auto;
  }
  .dlc {
    display: flex;
    justify-content: space-between;
    gap: var(--s2);
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
    color: var(--text-dim);
    font-variant-numeric: tabular-nums;
  }

  /* --- Stage --- */
  .stage {
    display: flex;
    flex-direction: column;
    min-height: 0;
    padding: var(--s4);
    gap: var(--s4);
  }
  .preview {
    flex: 1;
    display: grid;
    place-items: center;
    border: 1px solid var(--border);
    border-radius: var(--r);
    background:
      repeating-conic-gradient(#181b22 0% 25%, #14161c 0% 50%) 50% / 24px 24px;
    min-height: 0;
    overflow: hidden;
  }
  .preview img {
    max-width: 92%;
    max-height: 92%;
    object-fit: contain;
    filter: drop-shadow(0 12px 32px rgba(0, 0, 0, 0.55));
  }
  .placeholder {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--s2);
    text-align: center;
    padding: var(--s5);
  }
  .ph-icon {
    font-size: 34px;
    color: var(--border);
    line-height: 1;
  }
  .ph-title {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-dim);
  }
  .ph-sub {
    font-size: 12px;
    color: var(--muted);
    max-width: 260px;
  }

  /* --- Progress --- */
  .progress {
    display: flex;
    flex-direction: column;
    gap: var(--s2);
    opacity: 0.55;
    transition: opacity 0.2s;
  }
  .progress.on {
    opacity: 1;
  }
  .bar {
    height: 6px;
    background: var(--elev);
    border: 1px solid var(--border);
    border-radius: 999px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.25s ease;
  }
  .meta {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 12px;
    color: var(--muted);
    gap: var(--s3);
  }
  .meta .file {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta .count {
    flex-shrink: 0;
    display: flex;
    gap: var(--s2);
    font-variant-numeric: tabular-nums;
  }
  .meta .frac {
    color: var(--text-dim);
  }
  .meta .ok {
    color: var(--ok);
  }
  .meta .fail {
    color: var(--danger);
  }

  /* --- Gallery --- */
  .gallery {
    display: flex;
    flex-direction: column;
    gap: var(--s3);
    padding-top: var(--s4);
    border-top: 1px solid var(--border);
  }
  .galleryhead {
    display: flex;
    align-items: center;
    gap: var(--s3);
    font-size: 12px;
    color: var(--muted);
  }
  .galleryhead .gtitle {
    font-weight: 600;
    color: var(--text);
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .filter {
    flex: 1;
    min-width: 0;
    background: var(--elev);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text);
    padding: 6px 10px;
    font-size: 12px;
  }
  .filter:focus {
    outline: none;
    border-color: var(--accent);
  }
  .nomatch {
    padding: var(--s4);
    text-align: center;
  }
  .thumbs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(96px, 1fr));
    gap: var(--s3);
    max-height: 300px;
    overflow-y: auto;
    padding: var(--s3);
    border: 1px solid var(--border);
    border-radius: var(--r);
    background: var(--bg);
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: var(--s1);
    padding: var(--s1);
    border: 1px solid transparent;
    border-radius: var(--r-sm);
    transition:
      border-color 0.12s,
      background 0.12s;
  }
  .card:hover {
    background: var(--elev);
  }
  .card.active {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .thumb {
    aspect-ratio: 1;
    padding: 0;
    border: none;
    border-radius: 5px;
    overflow: hidden;
    cursor: pointer;
    background:
      repeating-conic-gradient(#181b22 0% 25%, #14161c 0% 50%) 50% / 10px 10px;
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

  /* --- Activity log --- */
  .logwrap {
    flex-shrink: 0;
  }
  .logwrap > summary {
    list-style: none;
    cursor: pointer;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
    padding-bottom: var(--s2);
    user-select: none;
  }
  .logwrap > summary::-webkit-details-marker {
    display: none;
  }
  .log {
    height: 120px;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: var(--r);
    background: var(--bg);
    padding: var(--s2) var(--s3);
    font-family: "Cascadia Code", Consolas, monospace;
    font-size: 11.5px;
    line-height: 1.6;
  }
  .logline {
    color: var(--text-dim);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .logline.muted {
    color: var(--muted);
  }
</style>
