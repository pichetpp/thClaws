// thClaws GUI Shell — bridge runtime, Tier 1.
//
// Injected automatically into every shell's <head> at HTML serve time.
// Exposes window.thclaws.* for the shell's code to call. Marshals
// JSON over postMessage to the parent React app (Mode A) or directly
// over WebSocket (Mode B, Tier 2 — not implemented yet).
//
// Tier 1 surface:
//   thclaws.shell.id          — string, this shell's id
//   thclaws.shell.sessionId   — string, the session this tab is bound to
//   thclaws.transport         — "tauri" | "ws"
//   thclaws.run(prompt, opts?) -> Promise<{ runId }>
//   thclaws.cancel(runId?)    -> void
//   thclaws.on(event, cb)     -> unsubscribe()
//       events: "text" | "done" | "error" | "ready"
//
// Tier 2 additions:
//   thclaws.storage.get(key)         -> Promise<any>     // file-backed
//   thclaws.storage.set(key, value)  -> Promise<void>    // <shell-root>/state/<sessionId>.json
//   thclaws.on(event, cb) events:    + "tool_call" + "tool_result"
//   thclaws.tools.invoke(name, args) -> Promise<string>  // Task 18 (separate)
//
// Host UI integration (full-screen escape hatch):
//   thclaws.ui.isFullscreen          — bool, host is showing us full-screen
//   thclaws.ui.onFullscreen(cb)      -> unsubscribe()    // fires with current state
//   thclaws.ui.exitFullscreen()      -> void             // ask host to leave full-screen
//   thclaws.ui.toggleFullscreen()    -> void             // enter/leave full-screen (⌘⇧U)
//   thclaws.ui.claimExitControl()    -> void             // hide the host's fallback chip
//   thclaws.ui.theme                 — "light" | "dark", host's resolved theme
//   thclaws.ui.onTheme(cb)           -> unsubscribe()    // fires with current theme
//   thclaws.ui.setTheme(mode)        -> void             // ask host to switch app theme
//   thclaws.ui.toggleTheme()         -> void             // flip light/dark
//                                    // (bridge also sets data-theme + color-scheme on <html>)
//
//   thclaws.model.get()              -> Promise<{provider, model, writable}>  // needs model.read
//   thclaws.model.list()             -> Promise<{current, groups:[{provider, models:[{id}]}]}>  // model.read (all providers)
//   thclaws.model.set(id)            -> Promise<{ok, model}>                   // needs model.write
//   thclaws.model.onChange(cb)       -> unsubscribe()     // fires when the active model changes
//   thclaws.model.current            — {provider, model} | null, last seen
//
//   thclaws.kms.list()               -> Promise<{kmss:[{name,scope,active}]}>   // needs kms.read
//   thclaws.kms.browse(name)         -> Promise<{kms, pages, sources}>          // needs kms.read
//   thclaws.research.list()          -> Promise<{jobs:[…]}>                     // needs research.read
//   thclaws.research.get(id)         -> Promise<{job|null}>                     // needs research.read

(() => {
  // Mode A URL: thclaws://localhost/gui-shell/<id>/<path>?session=<sid>
  // Mode B URL: https://host/t/<token>/<path>?session=<sid> — the
  // serve handler sets window.__thclaws_shell_mode = "ws" before this
  // runs, plus window.__thclaws_shell_ws_url for the WS endpoint.
  const url = new URL(location.href);
  const parts = url.pathname.split("/").filter(Boolean);
  const isModeB = window.__thclaws_shell_mode === "ws";
  // Identifier resolution:
  //   Mode B — the serve handler injects window.__thclaws_shell_id +
  //            window.__thclaws_shell_session_id at HTML render time
  //            (the URL `/t/<token>/` carries neither).
  //   Mode A — fall back to URL parts: /gui-shell/<id>/... + ?session=<id>
  const shellId =
    (typeof window.__thclaws_shell_id === "string" && window.__thclaws_shell_id) ||
    (parts[0] === "gui-shell" ? parts[1] : null);
  const sessionId =
    (typeof window.__thclaws_shell_session_id === "string" &&
      window.__thclaws_shell_session_id) ||
    url.searchParams.get("session");
  const transport = isModeB ? "ws" : "tauri";

  const pending = new Map();     // requestId -> {resolve, reject}
  const subscribers = new Map(); // eventName -> Set<callback>
  let nextRequestId = 1;

  // Mode B WebSocket transport — opened lazily on first send. The
  // bridge auto-reconnects with exponential backoff if the socket
  // drops mid-session (Risk 13 in dev-plan/33).
  let ws = null;
  let wsQueue = [];
  let wsBackoffMs = 500;
  function ensureWs() {
    if (!isModeB) return null;
    if (ws && ws.readyState === WebSocket.OPEN) return ws;
    if (ws && ws.readyState === WebSocket.CONNECTING) return ws;
    const wsUrl = (() => {
      const path = window.__thclaws_shell_ws_url || "/__ws";
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      return `${proto}//${location.host}${path}`;
    })();
    ws = new WebSocket(wsUrl);
    ws.addEventListener("open", () => {
      wsBackoffMs = 500;
      while (wsQueue.length) ws.send(wsQueue.shift());
    });
    ws.addEventListener("message", (evt) => {
      try {
        const obj = typeof evt.data === "string" ? JSON.parse(evt.data) : null;
        if (!obj) return;
        // Backend dispatches arrive as flat {type, ...} JSON. Convert
        // shell-relevant types into the bridge's ns="thclaws-shell-event"
        // envelope so the existing event-loop handler does the
        // routing.
        if (obj.type === "gui_shell_event") {
          handleShellEvent(obj);
        }
      } catch {}
    });
    ws.addEventListener("close", () => {
      const wait = Math.min(wsBackoffMs, 10_000);
      wsBackoffMs = Math.min(wsBackoffMs * 2, 30_000);
      setTimeout(ensureWs, wait);
    });
    return ws;
  }

  // Single point where backend gui_shell_event envelopes get fanned
  // out to bridge subscribers or resolve a pending request — shared
  // between Mode A (parent postMessage) and Mode B (WS).
  function handleShellEvent(data) {
    if (data.replyTo != null && pending.has(data.replyTo)) {
      const slot = pending.get(data.replyTo);
      pending.delete(data.replyTo);
      if (data.error) slot.reject(new Error(data.error));
      else slot.resolve(data.result);
      return;
    }
    if (data.event) {
      // Keep the convenience flag in sync before fanning out so a
      // subscriber that reads thclaws.ui.isFullscreen sees the new
      // value.
      if (data.event === "fullscreen" && window.thclaws && window.thclaws.ui) {
        window.thclaws.ui.isFullscreen = !!(data.payload && data.payload.active);
      }
      // Theme sync — the host pushes its resolved theme ("light" |
      // "dark") so shells can match the main UI instead of hardcoding
      // colors. We set `data-theme` + `color-scheme` on the shell's
      // root document directly so a shell only needs theme-aware CSS
      // (`:root[data-theme="light"]{…}`) — no JS required. Subscribers
      // of thclaws.ui.onTheme still fire below via the normal fanout.
      if (data.event === "theme" && window.thclaws && window.thclaws.ui) {
        const mode =
          data.payload && data.payload.mode === "light" ? "light" : "dark";
        window.thclaws.ui.theme = mode;
        try {
          const de = document.documentElement;
          de.setAttribute("data-theme", mode);
          de.style.colorScheme = mode;
        } catch (e) {
          /* document not ready / sandboxed — CSS default applies */
        }
      }
      // Cache the latest model so thclaws.model.onChange can replay it
      // and thclaws.model.current is always readable.
      if (data.event === "model" && window.thclaws && window.thclaws.model) {
        window.thclaws.model.current = data.payload || null;
      }
      const set = subscribers.get(data.event);
      if (set) {
        for (const cb of set) {
          try { cb(data.payload); } catch (err) {
            // eslint-disable-next-line no-console
            console.error("thclaws shell subscriber threw:", err);
          }
        }
      }
    }
  }

  function ensureSub(event) {
    let set = subscribers.get(event);
    if (!set) {
      set = new Set();
      subscribers.set(event, set);
    }
    return set;
  }

  function send(type, payload) {
    return new Promise((resolve, reject) => {
      const requestId = nextRequestId++;
      pending.set(requestId, { resolve, reject });
      if (isModeB) {
        // Mode B: write directly to WS, queuing until open.
        const frame = JSON.stringify({
          type: `gui_shell_${type}`,
          id: requestId,
          sessionId,
          shellId,
          ...payload,
        });
        const sock = ensureWs();
        if (sock && sock.readyState === WebSocket.OPEN) {
          sock.send(frame);
        } else {
          wsQueue.push(frame);
        }
        return;
      }
      // Mode A: parent React app marshals between window.ipc and us.
      parent.postMessage(
        {
          ns: "thclaws-shell",
          requestId,
          type,
          payload,
          shellId,
          sessionId,
        },
        "*",
      );
    });
  }

  // Mode A only: parent React app forwards backend dispatches to us
  // via postMessage. Mode B receives them directly on the WS, handled
  // in the ensureWs() message handler above.
  if (!isModeB) {
    window.addEventListener("message", (e) => {
      const data = e.data;
      if (!data || data.ns !== "thclaws-shell-event") return;
      handleShellEvent(data);
    });
  }

  window.thclaws = {
    shell: { id: shellId, sessionId },
    transport,

    run(prompt, opts) {
      if (typeof prompt !== "string") {
        return Promise.reject(new TypeError("thclaws.run: prompt must be a string"));
      }
      return send("run", { prompt, ...(opts || {}) });
    },

    cancel(runId) {
      // Fire-and-forget — cancel doesn't acknowledge.
      if (isModeB) {
        const frame = JSON.stringify({
          type: "gui_shell_cancel",
          id: nextRequestId++,
          sessionId,
          shellId,
          runId: runId || null,
        });
        const sock = ensureWs();
        if (sock && sock.readyState === WebSocket.OPEN) sock.send(frame);
        else wsQueue.push(frame);
        return;
      }
      parent.postMessage(
        {
          ns: "thclaws-shell",
          requestId: nextRequestId++,
          type: "cancel",
          payload: { runId: runId || null },
          shellId,
          sessionId,
        },
        "*",
      );
    },

    on(event, callback) {
      if (typeof callback !== "function") {
        throw new TypeError("thclaws.on: callback must be a function");
      }
      const set = ensureSub(event);
      set.add(callback);
      return () => set.delete(callback);
    },

    // Tier 2: resolve a path the agent produced (in `./output/...` or
    // similar) to a URL the browser can fetch — e.g. for
    //   <img src={thclaws.fileUrl(payload.file)}>
    //
    // Mode B: the bound shell's project root IS the cwd, so a relative
    // path like "output/abc.svg" maps to /t/<token>/file-asset/output/
    // abc.svg.
    //
    // Mode A: cwd is the launch dir (Tier 2.x — Task 21 adds CWD
    // switching). For now the shell author should ensure the agent
    // returns an absolute path in Mode A; relative paths return null.
    fileUrl(path) {
      if (typeof path !== "string" || !path) return null;
      if (isModeB) {
        const wsUrl = window.__thclaws_shell_ws_url || "";
        const prefix = wsUrl.endsWith("/__ws") ? wsUrl.slice(0, -5) : wsUrl;
        const tail = path.startsWith("/") ? path : "/" + path;
        return `${prefix}/file-asset${tail}`;
      }
      if (path.startsWith("/")) {
        return `thclaws://localhost/file-asset${path}`;
      }
      return null;
    },

    // Tier 2: direct tool invocation, bypasses the agent loop. Use
    // this for deterministic actions in a shell's UI ("Generate"
    // button calls image_gen, no model round-trip). Returns the
    // tool's raw string output.
    //
    // Read-only tools (ls / read / glob / grep / web_fetch / web_search
    // / kms_read / kms_search / docx_read / pdf_read / xlsx_read /
    // youtube_transcript / web_scrape / etc.) work directly.
    //
    // Mutating tools (Bash / Write / Edit / DocxCreate / etc.) reject
    // with "requires approval" — the approval flow lands in Tier 3.
    //
    // MCP-contributed tools aren't reachable here in Tier 2 (the IPC
    // arm builds a fresh built-ins-only ToolRegistry). Tier 3 routes
    // through the worker's registry so MCP tools work too.
    tools: {
      invoke(name, args) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.tools.invoke: name must be a non-empty string"),
          );
        }
        return send("tool_invoke", { name, args: args ?? null });
      },
    },

    // Tier 2: per-shell, per-session storage. Backed by a single JSON
    // file at <shell-root>/state/<sessionId>.json — atomic per-set,
    // namespaced by shell id (two shells with different ids cannot
    // read each other's storage even if they happen to share a session).
    storage: {
      get(key) {
        if (typeof key !== "string") {
          return Promise.reject(
            new TypeError("thclaws.storage.get: key must be a string"),
          );
        }
        return send("storage_get", { key });
      },
      set(key, value) {
        if (typeof key !== "string") {
          return Promise.reject(
            new TypeError("thclaws.storage.set: key must be a string"),
          );
        }
        return send("storage_set", { key, value });
      },
      // Tier 3: explicit delete. set(key, null) used to be the
      // convention; this is a clearer surface for shell authors.
      delete(key) {
        if (typeof key !== "string") {
          return Promise.reject(
            new TypeError("thclaws.storage.delete: key must be a string"),
          );
        }
        return send("storage_delete", { key });
      },
    },

    // ── dev-plan/39 Tier 3 — RPC + permissions surface ────────────
    //
    // Sugar over thclaws.tools.invoke that matches the dev-plan/39
    // documented contract: `await thclaws.callTool("Bash", {cmd:"ls"})`.
    // Identical wire format under the hood; the new name is what
    // marketplace shells should target going forward.
    callTool(name, args) {
      if (typeof name !== "string" || !name) {
        return Promise.reject(
          new TypeError("thclaws.callTool: name must be a non-empty string"),
        );
      }
      return send("tool_invoke", { name, args: args ?? null });
    },

    // Tier 3 stub. The shell asks for permission to take an action
    // and the user inline-approves via a custom widget (vs the full-
    // screen system modal). Engine wiring lands in Tier 3 follow-up;
    // for now, returning a clear rejection lets shells code against
    // the contract without crashing.
    awaitApproval(request) {
      return send("await_approval", request ?? {}).catch((e) => {
        if (String(e).includes("doesn't implement")) {
          throw new Error(
            "thclaws.awaitApproval: not yet wired through engine — falls back to the system approval modal. Tier 3 follow-up.",
          );
        }
        throw e;
      });
    },

    // Tier 3 stub. Streams turn events as an AsyncIterable. Until the
    // engine wires the per-event broadcast path, callers can keep
    // using thclaws.run() + thclaws.on("text", …) — this method is
    // here so marketplace shells coding to the new contract have a
    // stable entry point.
    streamTurn(prompt, opts) {
      const queue = [];
      const waiters = [];
      let done = false;
      let unsubText = null;
      let unsubDone = null;
      let unsubErr = null;
      const push = (item) => {
        if (waiters.length) waiters.shift()(item);
        else queue.push(item);
      };
      unsubText = window.thclaws.on("text", (p) => push({ done: false, value: { type: "text", delta: p.delta } }));
      unsubDone = window.thclaws.on("done", () => {
        done = true;
        push({ done: true, value: undefined });
        if (unsubText) unsubText();
        if (unsubErr) unsubErr();
      });
      unsubErr = window.thclaws.on("error", (p) => {
        done = true;
        push({ done: true, value: undefined, error: new Error(p?.message || "error") });
      });
      window.thclaws.run(prompt, opts).catch(() => {});
      return {
        [Symbol.asyncIterator]() {
          return {
            next() {
              if (queue.length) return Promise.resolve(queue.shift());
              if (done) return Promise.resolve({ done: true, value: undefined });
              return new Promise((r) => waiters.push(r));
            },
            return() {
              if (unsubText) unsubText();
              if (unsubDone) unsubDone();
              if (unsubErr) unsubErr();
              return Promise.resolve({ done: true, value: undefined });
            },
          };
        },
      };
    },

    // Tier 3 stub. Uploads a blob to the workspace's per-user asset
    // store + returns a `thclaws://localhost/file-asset/<rel>` URL.
    // Until the engine accepts uploads, falls back to a clear error
    // so shells can show "Upload not supported yet" inline.
    uploadFile(blob, name) {
      if (!(blob instanceof Blob)) {
        return Promise.reject(new TypeError("thclaws.uploadFile: first arg must be a Blob"));
      }
      return send("upload_file", {
        name: name || (blob.name || "upload.bin"),
        mime: blob.type || "application/octet-stream",
        // In Tier 3 follow-up the bridge will POST multipart to a
        // dedicated /upload route; the WS path is a fallback for
        // small payloads.
      }).catch((e) => {
        if (String(e).includes("doesn't implement")) {
          throw new Error(
            "thclaws.uploadFile: not yet wired through engine. Tier 3 follow-up.",
          );
        }
        throw e;
      });
    },

    // Host UI integration — full-screen control. In full-screen UI
    // mode the host hides all its chrome (tab bar, sidebar, status
    // bar) so the shell owns the viewport. The host always keeps a
    // keyboard escape (⌘⇧U / Ctrl⇧U) and a fallback exit affordance,
    // but a well-built shell should render its OWN exit control as
    // part of its chrome so the host's fallback never has to occlude
    // shell content. The reference pattern:
    //
    //   thclaws.ui.onFullscreen((active) => {
    //     myExitButton.hidden = !active;       // only meaningful in FS
    //     if (active) thclaws.ui.claimExitControl();  // hide host chip
    //   });
    //   myExitButton.onclick = () => thclaws.ui.exitFullscreen();
    //
    // `claimExitControl()` tells the host to suppress its own fallback
    // chip (the keyboard escape + a brief on-entry hint stay as the
    // safety net). Only call it once you've actually rendered a
    // working exit control of your own.
    ui: {
      // True while the host is showing this shell full-screen. Updated
      // from the host's `fullscreen` events; starts false.
      isFullscreen: false,

      // The host's resolved theme ("light" | "dark"), updated from the
      // host's `theme` events. The bridge also mirrors this onto
      // `document.documentElement[data-theme]` + `color-scheme`, so a
      // shell can theme purely in CSS. Starts "dark" (the historical
      // default) until the first host event arrives.
      theme: "dark",

      // Ask the host to leave full-screen UI mode (reveals the tab bar
      // etc.). No-op in Mode B (standalone `--serve --gui-shell`, where
      // the shell already owns the whole page and there's no chrome to
      // restore).
      exitFullscreen() {
        if (isModeB) return;
        parent.postMessage(
          { ns: "thclaws-shell", type: "hotkey", key: "exit-fullscreen-ui" },
          "*",
        );
      },

      // Toggle the host's full-screen UI (enter if windowed, leave if
      // full-screen) — the same action as the ⌘⇧U / Ctrl⇧U hotkey. No-op
      // in Mode B, where the shell already owns the whole page.
      toggleFullscreen() {
        if (isModeB) return;
        parent.postMessage(
          { ns: "thclaws-shell", type: "hotkey", key: "toggle-fullscreen-ui" },
          "*",
        );
      },

      // Ask the host to switch the app theme ("light" | "dark"). The host
      // persists it (same path as Settings), applies it app-wide, and
      // echoes the resolved theme back as a `theme` event so this shell
      // re-themes too. In Mode B there's no host to ask, so we flip the
      // shell document's own `data-theme` directly.
      setTheme(mode) {
        const next = mode === "light" ? "light" : "dark";
        if (isModeB) {
          window.thclaws.ui.theme = next;
          try {
            const de = document.documentElement;
            de.setAttribute("data-theme", next);
            de.style.colorScheme = next;
          } catch (e) {
            /* document not ready */
          }
          handleShellEvent({ event: "theme", payload: { mode: next } });
          return;
        }
        parent.postMessage(
          { ns: "thclaws-shell", type: "ui", key: "set-theme", mode: next },
          "*",
        );
      },

      // Convenience: flip between light and dark from the current theme.
      toggleTheme() {
        window.thclaws.ui.setTheme(
          window.thclaws.ui.theme === "dark" ? "light" : "dark",
        );
      },

      // Tell the host this shell renders its own exit control, so the
      // host can hide its fallback chip. Safe to call repeatedly.
      claimExitControl() {
        if (isModeB) return;
        parent.postMessage(
          { ns: "thclaws-shell", type: "ui", key: "exit-control-claimed" },
          "*",
        );
      },

      // Subscribe to full-screen state changes. Fires immediately with
      // the current state so callers don't miss the initial value.
      // Returns an unsubscribe function.
      onFullscreen(callback) {
        if (typeof callback !== "function") {
          throw new TypeError("thclaws.ui.onFullscreen: callback must be a function");
        }
        const unsub = window.thclaws.on("fullscreen", (p) =>
          callback(!!(p && p.active)),
        );
        // Replay current state on the next tick so subscribers added
        // before the first host event still get an initial call.
        Promise.resolve().then(() => callback(window.thclaws.ui.isFullscreen));
        return unsub;
      },

      // Subscribe to host theme changes. Fires immediately with the
      // current theme ("light" | "dark"). Most shells won't need this —
      // theme-aware CSS keyed on `:root[data-theme]` is enough — but
      // it's here for shells that style via JS (e.g. canvas/charts).
      onTheme(callback) {
        if (typeof callback !== "function") {
          throw new TypeError("thclaws.ui.onTheme: callback must be a function");
        }
        const unsub = window.thclaws.on("theme", (p) =>
          callback(p && p.mode === "light" ? "light" : "dark"),
        );
        Promise.resolve().then(() => callback(window.thclaws.ui.theme));
        return unsub;
      },
    },

    // Active-model widget surface. Gated host-side by the shell's
    // manifest permissions: `model.read` (get/list) and `model.write`
    // (set). Without them the calls reject — <thc-model> then renders
    // nothing. `set` changes the app-wide model (same as the sidebar
    // picker), so every shell's onChange fires.
    model: {
      // Last {provider, model} seen via a "model" event; null until one
      // arrives. onChange replays this on subscribe.
      current: null,

      get() {
        return send("model_get", {});
      },
      list() {
        return send("model_list", {});
      },
      set(id) {
        if (typeof id !== "string" || !id) {
          return Promise.reject(
            new TypeError("thclaws.model.set: id must be a non-empty string"),
          );
        }
        return send("model_set", { model: id });
      },

      // Fires when the active model changes (from this shell, another
      // shell, or the main sidebar). Replays the current value on the
      // next tick. Returns an unsubscribe function.
      onChange(callback) {
        if (typeof callback !== "function") {
          throw new TypeError("thclaws.model.onChange: callback must be a function");
        }
        const unsub = window.thclaws.on("model", (p) => callback(p));
        Promise.resolve().then(() => {
          if (window.thclaws.model.current) callback(window.thclaws.model.current);
        });
        return unsub;
      },
    },

    // Deterministic knowledge-base API (needs `kms.read`). No LLM — reads
    // the KMS store directly instead of prompting the agent.
    kms: {
      // -> { kmss: [{ name, scope, active }] }
      list() {
        return send("kms_list", {});
      },
      // -> { kms, pages:[{…}], sources:[{…}] }
      browse(name) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.kms.browse: name must be a non-empty string"),
          );
        }
        return send("kms_browse", { name: name });
      },
    },

    // Deterministic research-job API (needs `research.read`). No LLM —
    // reads the live job registry (running + recently-completed), the real
    // source of {status, score, phase, …}.
    research: {
      // -> { jobs: [{ id, query, status, phase, iterations_done,
      //              source_count, score, kms_target, result_page, error }] }
      list() {
        return send("research_list", {});
      },
      // -> { job: {…} | null }
      get(id) {
        return send("research_get", { jobId: id });
      },
    },

    // Tier 3: read-only access to the shell's declared permissions
    // (from shell.json). Lets shell code disable UI for actions the
    // user didn't grant rather than calling and getting denied.
    permissions: {
      list() {
        return send("permissions_list", {});
      },
      has(action) {
        return window.thclaws.permissions
          .list()
          .then((list) => Array.isArray(list) && list.includes(action))
          .catch(() => false);
      },
    },
  };

  if (isModeB) {
    // Open the WS proactively so the first send doesn't pay the
    // connection setup latency.
    ensureWs();
  } else {
    // Mode A only — signal to the parent React app.
    parent.postMessage(
      { ns: "thclaws-shell", type: "ready", shellId, sessionId },
      "*",
    );
    // Forward parent-app hotkeys that the iframe's focus would
    // otherwise swallow. The full-screen-UI toggle (⌘⇧U / Ctrl⇧U)
    // lives in the parent React app, so the parent's
    // window.addEventListener("keydown") never fires while the user
    // is typing inside the shell. Posting a `hotkey` envelope lets
    // the parent run its handler regardless of focus.
    window.addEventListener(
      "keydown",
      (e) => {
        const isMac =
          typeof navigator !== "undefined" &&
          navigator.platform.startsWith("Mac");
        const modOk = isMac
          ? e.metaKey && !e.ctrlKey && !e.altKey && e.shiftKey
          : e.ctrlKey && !e.metaKey && !e.altKey && e.shiftKey;
        if (!modOk) return;
        const key = (e.key || "").toLowerCase();
        if (key !== "u") return;
        e.preventDefault();
        e.stopImmediatePropagation();
        parent.postMessage(
          { ns: "thclaws-shell", type: "hotkey", key: "toggle-fullscreen-ui" },
          "*",
        );
      },
      { capture: true },
    );
  }
})();
