// Media Studio — GUI Shell for image + video generation (dev-plan/40 Tier 3).
//
// Drives the built-in media tools directly via thclaws.callTool():
//   text2image / image2image → TextToImage / ImageToImage (sync; returns a path)
//   text2video / image2video → TextToVideo / ImageToVideo (async; returns a
//     job_id) then polls MediaJobStatus until the clip is ready.
// Results are collected in a per-session gallery (persisted via
// thclaws.storage) with a lightbox. The submit tools require approval, so
// clicking Generate raises the normal approval modal (the engine routes
// gui_shell_tool_invoke through the same GuiApprover as the agent).

const MODELS = {
  text2image: [
    { value: "flash", label: "Gemini 3.1 Flash (fast)" },
    { value: "pro", label: "Gemini 3.1 Pro" },
    { value: "gpt-image-2", label: "OpenAI GPT Image 2" },
    { value: "qwen-image-2.0", label: "Qwen Image 2.0" },
    { value: "qwen-image-2.0-pro", label: "Qwen Image 2.0 Pro" },
  ],
  image2image: [
    { value: "flash", label: "Gemini 3.1 Flash (fast)" },
    { value: "pro", label: "Gemini 3.1 Pro" },
    { value: "gpt-image-2", label: "OpenAI GPT Image 2" },
    { value: "qwen-image-2.0", label: "Qwen Image 2.0" },
    { value: "qwen-image-2.0-pro", label: "Qwen Image 2.0 Pro" },
  ],
  text2video: [
    { value: "fast", label: "Veo 3.1 Fast" },
    { value: "quality", label: "Veo 3.1" },
    { value: "lite", label: "Veo 3.1 Lite" },
    { value: "happyhorse-1.0-t2v", label: "HappyHorse 1.0 (DashScope)" },
  ],
  image2video: [
    { value: "fast", label: "Veo 3.1 Fast" },
    { value: "quality", label: "Veo 3.1" },
    { value: "lite", label: "Veo 3.1 Lite" },
    { value: "happyhorse-1.0-i2v", label: "HappyHorse 1.0 (DashScope)" },
  ],
};

const TOOL = {
  text2image: "TextToImage",
  image2image: "ImageToImage",
  text2video: "TextToVideo",
  image2video: "ImageToVideo",
};

const META_KEY = "media-meta"; // filename → {prompt, model} for things we generated
const MAX_GALLERY = 200;
const POLL_INTERVAL_MS = 6000;
const POLL_MAX_MS = 10 * 60 * 1000;

const ASSET_RE = /(?:^|[\s(])((?:\.?\/)?\S*output\/\S+\.(?:png|jpe?g|webp|mp4|webm))/i;
const JOBID_RE = /job_id=([A-Za-z0-9-]+)/;
const MEDIA_RE = /\.(png|jpe?g|webp|gif|mp4|webm|mov|m4v)$/i;
const VIDEO_RE = /\.(mp4|webm|mov|m4v)$/i;
const TS_RE = /(\d{8}-\d{6})/; // img-YYYYMMDD-HHMMSS- … (sort key)
const basename = (p) => String(p).split("/").pop();

let mode = "text2image";
let gallery = []; // displayed items, rebuilt from disk: [{type, path, prompt, model}]
let meta = {}; // filename → {prompt, model} for items generated this session

// ── elements ─────────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const modelSel = $("model");
const inputPathField = $("input-path-field");
const inputPath = $("input-path");
const promptEl = $("prompt");
const aspectSel = $("aspect");
const sizeField = $("size-field");
const durationField = $("duration-field");
const durationSel = $("duration");
const resolutionField = $("resolution-field");
const generateBtn = $("generate");
const statusEl = $("status");
const hintEl = $("hint");
const galleryEl = $("gallery");
const galleryEmpty = $("gallery-empty");

$("transport-badge").textContent = `${thclaws.transport} · ${thclaws.shell.sessionId ?? "(no session)"}`;

// ── asset URL (works in both Mode A desktop + Mode B serve) ──────────
// The bridge's fileUrl returns null for relative paths in Mode A, but the
// desktop file-asset handler resolves workspace-relative paths fine — so
// fall back to the protocol URL directly.
function assetUrl(path) {
  if (!path) return null;
  const viaBridge = thclaws.fileUrl(path);
  if (viaBridge) return viaBridge;
  const tail = path.startsWith("/") ? path : "/" + path;
  return `thclaws://localhost/file-asset${tail}`;
}

function extractPath(result) {
  const m = String(result || "").match(ASSET_RE);
  return m ? m[1].replace(/^\.\//, "") : null;
}

// ── mode UI ──────────────────────────────────────────────────────────
function isVideo() {
  return mode === "text2video" || mode === "image2video";
}
function needsInput() {
  return mode === "image2image" || mode === "image2video";
}

function applyMode() {
  document.querySelectorAll(".mode-tab").forEach((t) =>
    t.classList.toggle("active", t.dataset.mode === mode),
  );
  modelSel.innerHTML = "";
  for (const m of MODELS[mode]) {
    const o = document.createElement("option");
    o.value = m.value;
    o.textContent = m.label;
    modelSel.appendChild(o);
  }
  inputPathField.hidden = !needsInput();
  sizeField.hidden = isVideo();
  durationField.hidden = !isVideo();
  resolutionField.hidden = !isVideo();
  // Video only supports 16:9 / 9:16 — disable the others.
  [...aspectSel.options].forEach((o) => {
    o.disabled = isVideo() && !["16:9", "9:16"].includes(o.value);
  });
  if (isVideo() && !["16:9", "9:16"].includes(aspectSel.value)) aspectSel.value = "16:9";
  hintEl.textContent = isVideo()
    ? "Video renders asynchronously (~30–120s). Veo ≈ $3–6 per clip."
    : "";
  setStatus("");
}

function setStatus(text, kind) {
  statusEl.textContent = text;
  statusEl.className = "status" + (kind ? " " + kind : "");
}

function setBusy(busy) {
  generateBtn.disabled = busy;
  generateBtn.textContent = busy ? "Working…" : "Generate";
}

// ── gallery (disk-backed: shows everything under output/) ─────────────
// Scan output/ via the read-only Glob tool and rebuild the grid from
// what's actually on disk — so pre-existing media (from earlier sessions,
// the agent, or other tools) shows too, not just this session's output.
// Session metadata (prompt/model) is overlaid by filename.
async function refreshGallery() {
  let listing = "";
  try {
    listing = await thclaws.callTool("Glob", { pattern: "**/*", path: "output" });
  } catch {
    listing = ""; // output/ doesn't exist yet
  }
  const paths = String(listing || "")
    .split("\n")
    .map((s) => s.trim())
    .filter((p) => p && MEDIA_RE.test(p));
  // Newest first by the embedded timestamp (falls back to path order).
  paths.sort((a, b) => {
    const ka = (a.match(TS_RE) || [a])[0];
    const kb = (b.match(TS_RE) || [b])[0];
    return ka < kb ? 1 : ka > kb ? -1 : 0;
  });
  gallery = paths.slice(0, MAX_GALLERY).map((path) => {
    const m = meta[basename(path)] || {};
    return {
      path,
      type: VIDEO_RE.test(path) ? "video" : "image",
      prompt: m.prompt || "",
      model: m.model || "",
    };
  });
  renderGallery();
}

function renderGallery() {
  [...galleryEl.querySelectorAll(".card")].forEach((c) => c.remove());
  galleryEmpty.hidden = gallery.length > 0;
  for (const item of gallery) {
    galleryEl.appendChild(makeCard(item));
  }
}

function makeCard(item) {
  const card = document.createElement("div");
  card.className = "card";
  const url = assetUrl(item.path);
  const badge = `<span class="badge">${item.type === "video" ? "▶ video" : "image"}</span>`;
  const cap = escapeHtml(item.prompt || basename(item.path));
  if (item.type === "video") {
    card.innerHTML = `${badge}<video src="${url}" muted preload="metadata"></video><div class="cap">${cap}</div>`;
  } else {
    card.innerHTML = `${badge}<img src="${url}" alt="" loading="lazy"><div class="cap">${cap}</div>`;
  }
  card.addEventListener("click", () => {
    // In an input-needing mode (Image Edit / Image → Video), clicking an
    // image card picks it as the source frame; otherwise (and for video
    // cards) open the lightbox.
    if (needsInput() && item.type === "image") {
      inputPath.value = item.path;
      setStatus(`Source set: ${basename(item.path)}`, "ok");
    } else {
      openLightbox(item);
    }
  });
  return card;
}

// Remember prompt/model for a file we just generated, keyed by filename
// so it survives the abs-vs-relative path difference from Glob.
function recordMeta(path, info) {
  meta[basename(path)] = info;
  thclaws.storage.set(META_KEY, meta).catch(() => {});
}

function escapeHtml(s) {
  return String(s || "").replace(/[&<>"]/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
  );
}

// ── lightbox ─────────────────────────────────────────────────────────
const lightbox = $("lightbox");
const lightboxBody = $("lightbox-body");
const lightboxCaption = $("lightbox-caption");

function openLightbox(item) {
  const url = assetUrl(item.path);
  lightboxBody.innerHTML =
    item.type === "video"
      ? `<video src="${url}" controls autoplay></video>`
      : `<img src="${url}" alt="">`;
  lightboxCaption.textContent = [
    [item.model, item.path].filter(Boolean).join(" · "),
    item.prompt,
  ]
    .filter(Boolean)
    .join("\n");
  lightbox.hidden = false;
}
function closeLightbox() {
  lightbox.hidden = true;
  lightboxBody.innerHTML = "";
}
$("lightbox-close").addEventListener("click", closeLightbox);
lightbox.addEventListener("click", (e) => {
  if (e.target === lightbox) closeLightbox();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !lightbox.hidden) closeLightbox();
});

// ── generate ─────────────────────────────────────────────────────────
async function generate() {
  const prompt = promptEl.value.trim();
  if (!prompt) {
    setStatus("Enter a prompt first.", "error");
    return;
  }
  const model = modelSel.value;
  const args = { prompt, model, aspect_ratio: aspectSel.value };
  if (needsInput()) {
    const p = inputPath.value.trim();
    if (!p) {
      setStatus("Pick or type a source image path.", "error");
      return;
    }
    args.input_path = p;
  }
  if (isVideo()) {
    args.duration = parseInt(durationSel.value, 10);
    args.resolution = $("resolution").value;
  } else {
    args.size = sizeField.hidden ? "1K" : $("size").value;
  }

  setBusy(true);
  try {
    setStatus("Submitting… (approve the spend if prompted)");
    const result = await thclaws.callTool(TOOL[mode], args);
    if (isVideo()) {
      await handleVideoResult(result, prompt, model);
    } else {
      const path = extractPath(result);
      if (!path) throw new Error(result || "no image path in result");
      recordMeta(path, { prompt, model });
      await refreshGallery();
      setStatus("Done.", "ok");
    }
  } catch (e) {
    setStatus(String(e && e.message ? e.message : e), "error");
  } finally {
    setBusy(false);
  }
}

async function handleVideoResult(submitResult, prompt, model) {
  const m = String(submitResult || "").match(JOBID_RE);
  if (!m) throw new Error(submitResult || "no job_id in submit result");
  const jobId = m[1];
  setStatus(`Rendering ${jobId}…`);
  const card = pendingCard(`Rendering ${jobId}…`);
  galleryEmpty.hidden = true;
  galleryEl.prepend(card);

  const started = Date.now();
  while (Date.now() - started < POLL_MAX_MS) {
    await sleep(POLL_INTERVAL_MS);
    let status;
    try {
      status = await thclaws.callTool("MediaJobStatus", { job_id: jobId });
    } catch (e) {
      card.remove();
      throw e;
    }
    const s = String(status || "");
    if (s.startsWith("done")) {
      card.remove();
      const path = extractPath(s);
      if (!path) throw new Error("done but no video path: " + s);
      recordMeta(path, { prompt, model });
      await refreshGallery();
      setStatus("Done.", "ok");
      return;
    }
    if (s.startsWith("failed")) {
      card.remove();
      throw new Error(s);
    }
    setStatus(s || `Rendering ${jobId}…`);
  }
  card.remove();
  throw new Error(`Timed out waiting for ${jobId}`);
}

function pendingCard(text) {
  const c = document.createElement("div");
  c.className = "card pending";
  c.textContent = text;
  return c;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── wiring ───────────────────────────────────────────────────────────
$("modes").addEventListener("click", (e) => {
  const tab = e.target.closest(".mode-tab");
  if (!tab) return;
  mode = tab.dataset.mode;
  applyMode();
});
generateBtn.addEventListener("click", generate);
promptEl.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === "Enter") generate();
});
// Re-scan output/ (the gallery is disk-backed; we don't delete files).
$("clear-gallery").addEventListener("click", () => {
  refreshGallery();
});

// Full-screen exit control (mirrors chatbot shell).
(() => {
  const exitBtn = $("exit-fullscreen");
  if (!exitBtn || !thclaws.ui) return;
  exitBtn.addEventListener("click", () => thclaws.ui.exitFullscreen());
  thclaws.ui.onFullscreen((active) => {
    exitBtn.hidden = !active;
    if (active) thclaws.ui.claimExitControl();
  });
})();

// ── init ─────────────────────────────────────────────────────────────
applyMode();
// Load session metadata (prompt/model captions), then scan output/ so the
// gallery shows pre-existing media immediately.
thclaws.storage
  .get(META_KEY)
  .then((saved) => {
    if (saved && typeof saved === "object") meta = saved;
  })
  .catch(() => {})
  .finally(() => {
    refreshGallery();
  });
