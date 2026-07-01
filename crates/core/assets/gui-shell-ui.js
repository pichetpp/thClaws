// thClaws GUI Shell — shared chrome runtime.
//
// Injected automatically into every shell's <head> at serve time (after
// the bridge runtime), so shells get a consistent navbar/header without
// re-implementing the bridge-status indicator, the full-screen toggle, or
// the theme toggle that every studio used to hand-roll.
//
// Registers <thc-header>. Light DOM (no shadow root) so the shared
// theme.css styles it and studios can override.
//
//   <thc-header label="Research Console">
//     <svg slot="icon" ...>…</svg>                 <!-- optional, preserved -->
//     <button slot="actions" id="search" title="Search">…</button>
//   </thc-header>
//
//   const h = document.querySelector("thc-header");
//   h.setStatus("researching…");   // right-aligned status text
//
// Right side, left → right:  [status] [bridge] [your slot="actions" …]
//                            [theme] [full-screen]
// The two standard controls are ALWAYS pinned far-right; a shell author's
// own `slot="actions"` buttons sit just left of them (wire their clicks in
// your own main.js — the component only positions + styles them). Action
// buttons with no class get `.thc-iconbtn` so they match by default.
//
// Standard controls, wired from window.thclaws.ui:
//   • bridge-status pill (connected · <transport> / bridge unavailable)
//   • full-screen toggle  — enter/leave the host's full-screen UI (⌘⇧U)
//   • theme toggle        — flip app light/dark; shown when the host's own
//                           theme control is out of reach (full-screen or
//                           a standalone --serve --gui-shell page)
(function () {
  "use strict";

  if (typeof window === "undefined" || !window.customElements) return;
  if (customElements.get("thc-header")) return;

  function svg(inner) {
    return (
      '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" ' +
      'stroke="currentColor" stroke-width="2" stroke-linecap="round" ' +
      'stroke-linejoin="round" aria-hidden="true">' +
      inner +
      "</svg>"
    );
  }

  var IC_MAXIMIZE = svg(
    '<polyline points="15 3 21 3 21 9"/><polyline points="9 21 3 21 3 15"/>' +
      '<line x1="21" y1="3" x2="14" y2="10"/><line x1="3" y1="21" x2="10" y2="14"/>',
  );
  var IC_MINIMIZE = svg(
    '<polyline points="4 14 10 14 10 20"/><polyline points="20 10 14 10 14 4"/>' +
      '<line x1="14" y1="10" x2="21" y2="3"/><line x1="3" y1="21" x2="10" y2="14"/>',
  );
  var IC_SUN = svg(
    '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41' +
      'M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/>',
  );
  var IC_MOON = svg('<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>');

  function bridge() {
    return typeof window.thclaws !== "undefined" ? window.thclaws : null;
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"]/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c];
    });
  }

  // Shorten a model id for display by dropping a leading "<provider>/"
  // (the full id is still the option value). Mirrors the main-app picker.
  function stripPrefix(id, provider) {
    var aliases = { "ollama-anthropic": "oa", "openai-compat": "oai" };
    var cands = [provider, aliases[provider]];
    for (var i = 0; i < cands.length; i++) {
      var p = cands[i];
      if (p && id.indexOf(p + "/") === 0) return id.slice(p.length + 1);
    }
    return id;
  }

  // For a custom element the HTML parser created, connectedCallback fires
  // BEFORE its light-DOM children are parsed — so slot relocation has to
  // wait until the document finishes parsing.
  function whenChildrenReady(el, cb) {
    var doc = el.ownerDocument || document;
    if (doc.readyState === "loading") {
      doc.addEventListener("DOMContentLoaded", cb, { once: true });
    } else {
      cb();
    }
  }

  class ThcHeader extends HTMLElement {
    connectedCallback() {
      if (this._init) return;
      this._init = true;
      whenChildrenReady(this, this._render.bind(this));
    }

    _render() {
      if (this._rendered) return;
      this._rendered = true;

      var label = this.getAttribute("label") || document.title || "";
      // Capture slotted children before we overwrite innerHTML (references
      // survive the detach; their event listeners come along on re-insert).
      var iconNode = this.querySelector('[slot="icon"]');
      var actionNodes = [];
      var list = this.querySelectorAll('[slot="actions"]');
      for (var i = 0; i < list.length; i++) actionNodes.push(list[i]);

      this.innerHTML =
        '<div class="thc-header-left">' +
        '<span class="thc-header-icon"></span>' +
        '<span class="thc-header-title">' +
        escapeHtml(label) +
        "</span></div>" +
        '<div class="thc-header-right">' +
        '<span class="thc-status"></span>' +
        '<span class="thc-bridge">connecting…</span>' +
        '<span class="thc-header-actions"></span>' +
        '<button class="thc-iconbtn thc-theme-toggle" type="button" hidden ' +
        'title="Toggle light / dark"></button>' +
        '<button class="thc-iconbtn thc-fs-toggle" type="button" hidden ' +
        'title="Toggle full screen (⌘⇧U)"></button>' +
        "</div>";

      this._statusEl = this.querySelector(".thc-status");
      this._bridgeEl = this.querySelector(".thc-bridge");
      this._themeBtn = this.querySelector(".thc-theme-toggle");
      this._fsBtn = this.querySelector(".thc-fs-toggle");

      if (iconNode) {
        iconNode.removeAttribute("slot");
        this.querySelector(".thc-header-icon").appendChild(iconNode);
      }

      var actionsHost = this.querySelector(".thc-header-actions");
      for (var j = 0; j < actionNodes.length; j++) {
        var n = actionNodes[j];
        n.removeAttribute("slot");
        if (n.tagName === "BUTTON" && !n.className) n.className = "thc-iconbtn";
        actionsHost.appendChild(n);
      }

      if (this._pendingStatus != null) {
        this._statusEl.textContent = this._pendingStatus;
      }
      this._wireBridge();
    }

    _wireBridge() {
      var t = bridge();
      if (!t) {
        this._setBridge("bridge unavailable", "err");
        return;
      }
      this._setBridge("connected · " + (t.transport || "unknown"), "ok");

      var ui = t.ui;
      if (!ui) return;
      var self = this;
      // Mode B (standalone --serve --gui-shell) is always full-screen-ish
      // and has no host chrome to toggle.
      var standalone = t.transport === "ws";

      if (this._fsBtn && !standalone && typeof ui.toggleFullscreen === "function") {
        this._fsBtn.hidden = false;
        this._fsBtn.innerHTML = IC_MAXIMIZE;
        this._fsBtn.addEventListener("click", function () {
          ui.toggleFullscreen();
        });
      }

      if (this._themeBtn && typeof ui.toggleTheme === "function") {
        this._themeBtn.addEventListener("click", function () {
          ui.toggleTheme();
        });
        if (standalone) this._themeBtn.hidden = false;
      }

      this._setThemeIcon(ui.theme === "light" ? "light" : "dark");
      if (typeof ui.onTheme === "function") {
        ui.onTheme(function (mode) {
          self._setThemeIcon(mode);
        });
      }
      if (typeof ui.onFullscreen === "function") {
        ui.onFullscreen(function (active) {
          if (self._fsBtn && !standalone) {
            self._fsBtn.innerHTML = active ? IC_MINIMIZE : IC_MAXIMIZE;
          }
          // Reveal the theme toggle while the host chrome is hidden.
          if (self._themeBtn && typeof ui.toggleTheme === "function") {
            self._themeBtn.hidden = !(active || standalone);
          }
          if (active && typeof ui.claimExitControl === "function") {
            ui.claimExitControl();
          }
        });
      }
    }

    _setBridge(text, cls) {
      if (this._bridgeEl) {
        this._bridgeEl.textContent = text;
        this._bridgeEl.className = "thc-bridge " + (cls || "");
      }
    }

    _setThemeIcon(mode) {
      if (!this._themeBtn) return;
      // Show the icon of the *current* theme (sun = light, moon = dark).
      this._themeBtn.innerHTML = mode === "light" ? IC_SUN : IC_MOON;
    }

    setStatus(text) {
      this._pendingStatus = text == null ? "" : text;
      if (this._statusEl) this._statusEl.textContent = this._pendingStatus;
    }

    set status(v) {
      this.setStatus(v);
    }
    get status() {
      return this._statusEl ? this._statusEl.textContent : this._pendingStatus || "";
    }
  }

  customElements.define("thc-header", ThcHeader);

  // <thc-model> — active-model widget. Behaviour derives entirely from
  // the shell's manifest permissions (the host enforces them):
  //   model.write → a <select> to switch the model
  //   model.read  → a read-only badge of the current model
  //   neither     → renders nothing (model.get() rejects)
  // Drop it anywhere, e.g. inside <thc-header slot="actions">.
  function formatCtx(n) {
    if (!n || n <= 0) return "";
    if (n >= 1000000) return (n / 1000000).toFixed(1).replace(/\.0$/, "") + "M";
    if (n >= 1000) return Math.round(n / 1000) + "k";
    return String(n);
  }

  class ThcModel extends HTMLElement {
    connectedCallback() {
      if (this._init) return;
      this._init = true;
      whenChildrenReady(this, this._render.bind(this));
    }

    async _render() {
      if (this._rendered) return;
      this._rendered = true;
      var t = bridge();
      if (!t || !t.model) {
        this.hidden = true;
        return;
      }
      var info;
      try {
        info = await t.model.get();
      } catch (e) {
        this.hidden = true; // no model.read permission
        return;
      }
      if (!info || !info.model) {
        this.hidden = true;
        return;
      }
      this.hidden = false;
      this._current = info.model;
      this._provider = info.provider || "";
      var self = this;
      if (info.writable) {
        this._buildPicker();
      } else {
        this.innerHTML =
          '<span class="thc-model-badge" title="Active model"></span>';
        this._badge = this.querySelector(".thc-model-badge");
        this._badge.textContent = info.model;
      }
      if (typeof t.model.onChange === "function") {
        t.model.onChange(function (p) {
          self._onModel(p);
        });
      }
    }

    // A trigger row + a search-filtered, provider-grouped dropdown —
    // the same shape as the main-app sidebar model picker.
    _buildPicker() {
      this.innerHTML =
        '<button type="button" class="thc-model-trigger" title="Change model">' +
        '<span class="thc-model-provider"></span>' +
        '<span class="thc-model-current"></span>' +
        '<span class="thc-model-caret">▾</span>' +
        "</button>" +
        '<div class="thc-model-panel" hidden>' +
        '<div class="thc-model-search">' +
        '<input type="text" class="thc-model-search-input" autocomplete="off" ' +
        'autocorrect="off" autocapitalize="off" spellcheck="false" ' +
        'placeholder="Loading…" />' +
        "</div>" +
        '<div class="thc-model-list"></div>' +
        "</div>";
      this._trigger = this.querySelector(".thc-model-trigger");
      this._panel = this.querySelector(".thc-model-panel");
      this._search = this.querySelector(".thc-model-search-input");
      this._list = this.querySelector(".thc-model-list");
      this._query = "";
      this._groups = null;
      this._updateTrigger();

      var self = this;
      this._trigger.addEventListener("click", function () {
        self._toggle();
      });
      this._search.addEventListener("input", function () {
        self._query = self._search.value;
        self._renderList();
      });
      this._onDocDown = function (e) {
        if (!self.contains(e.target)) self._close();
      };
      this._onKey = function (e) {
        if (e.key === "Escape") self._close();
      };
    }

    _updateTrigger() {
      var provEl = this.querySelector(".thc-model-provider");
      var curEl = this.querySelector(".thc-model-current");
      if (provEl) provEl.textContent = this._provider;
      if (curEl) curEl.textContent = stripPrefix(this._current, this._provider);
    }

    _toggle() {
      if (this._panel.hidden) this._open();
      else this._close();
    }

    async _open() {
      this._panel.hidden = false;
      document.addEventListener("mousedown", this._onDocDown);
      document.addEventListener("keydown", this._onKey);
      this._search.focus();
      if (!this._groups) {
        try {
          var r = await bridge().model.list();
          this._groups = (r && r.groups) || [];
        } catch (e) {
          this._groups = [];
        }
      }
      var total = this._groups.reduce(function (a, g) {
        return a + ((g.models || []).length);
      }, 0);
      this._search.placeholder =
        "Search " + total + " model" + (total === 1 ? "" : "s") + "…";
      this._renderList();
    }

    _close() {
      if (!this._panel || this._panel.hidden) return;
      this._panel.hidden = true;
      document.removeEventListener("mousedown", this._onDocDown);
      document.removeEventListener("keydown", this._onKey);
    }

    _renderList() {
      if (!this._list) return;
      if (!this._groups) {
        this._list.innerHTML =
          '<div class="thc-model-empty">Loading models…</div>';
        return;
      }
      var q = (this._query || "").trim().toLowerCase();
      var groups = this._groups
        .map(function (g) {
          if (!q) return g;
          var models = (g.models || []).filter(function (m) {
            return (
              m.id.toLowerCase().indexOf(q) >= 0 ||
              g.provider.toLowerCase().indexOf(q) >= 0
            );
          });
          return { provider: g.provider, tier: g.tier, models: models };
        })
        .filter(function (g) {
          return (g.models || []).length > 0;
        });

      if (!groups.length) {
        this._list.innerHTML =
          '<div class="thc-model-empty">No models match.</div>';
        return;
      }
      var self = this;
      var html = "";
      var prevTier = null;
      groups.forEach(function (g) {
        var tierLabel =
          g.tier === "featured"
            ? "Featured"
            : g.tier === "additional"
              ? "Additional"
              : null;
        if (tierLabel && g.tier !== prevTier) {
          html += '<div class="thc-model-tier">' + escapeHtml(tierLabel) + "</div>";
        }
        prevTier = g.tier;
        html += '<div class="thc-model-group">' + escapeHtml(g.provider) + "</div>";
        (g.models || []).forEach(function (m) {
          var isCur = m.id === self._current;
          var ctx = formatCtx(m.context);
          html +=
            '<button type="button" class="thc-model-row' +
            (isCur ? " is-current" : "") +
            '" data-id="' +
            escapeHtml(m.id) +
            '">' +
            '<span class="thc-model-row-id">' +
            escapeHtml(stripPrefix(m.id, g.provider)) +
            "</span>" +
            (ctx
              ? '<span class="thc-model-row-ctx">' + escapeHtml(ctx) + "</span>"
              : "") +
            "</button>";
        });
      });
      this._list.innerHTML = html;
      var rows = this._list.querySelectorAll(".thc-model-row");
      for (var k = 0; k < rows.length; k++) {
        rows[k].addEventListener("click", function () {
          bridge().model.set(this.getAttribute("data-id"));
          self._close();
        });
      }
    }

    _onModel(p) {
      var model = p && p.model;
      if (!model) return;
      this._current = model;
      if (p.provider) this._provider = p.provider;
      if (this._badge) this._badge.textContent = model;
      if (this._trigger) {
        this._updateTrigger();
        if (this._panel && !this._panel.hidden) this._renderList();
      }
    }
  }

  customElements.define("thc-model", ThcModel);

  // <thc-sidebar> — standard agent sidebar layout. The model picker is
  // pinned at the TOP automatically (it hides itself when the shell has no
  // model.* permission, so "if the agent uses it" is handled for free);
  // everything the author puts inside becomes the scrollable body below.
  //
  //   <thc-sidebar>
  //     <div class="rail-header">…</div>
  //     <div class="rail-body">…</div>
  //   </thc-sidebar>
  //
  // Opt out of the auto model picker with the `no-model` attribute.
  class ThcSidebar extends HTMLElement {
    connectedCallback() {
      if (this._init) return;
      this._init = true;
      whenChildrenReady(this, this._render.bind(this));
    }

    _render() {
      if (this._rendered) return;
      this._rendered = true;

      // Move the author's content aside before we lay out the shell.
      var body = [];
      while (this.firstChild) body.push(this.removeChild(this.firstChild));

      if (!this.hasAttribute("no-model")) {
        var picker = document.createElement("thc-model");
        picker.className = "thc-sidebar-model";
        this.appendChild(picker);
      }

      var bodyWrap = document.createElement("div");
      bodyWrap.className = "thc-sidebar-body";
      for (var i = 0; i < body.length; i++) bodyWrap.appendChild(body[i]);
      this.appendChild(bodyWrap);
    }
  }

  customElements.define("thc-sidebar", ThcSidebar);

  // ── Minimal markdown → HTML (shared by <thc-chat>) ───────────────────
  // Covers what agents actually emit: headers, bold/italic, code spans,
  // fenced code, lists, links, paragraphs. Intentionally small.
  function chatGroupList(src, pattern, tag) {
    var lines = src.split("\n");
    var out = [];
    var inList = false;
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      var m = /^[-\d]/.test(line) ? line.match(new RegExp(pattern)) : null;
      if (m) {
        if (!inList) {
          out.push("<" + tag + ">");
          inList = true;
        }
        out.push("<li>" + m[1] + "</li>");
      } else {
        if (inList) {
          out.push("</" + tag + ">");
          inList = false;
        }
        out.push(line);
      }
    }
    if (inList) out.push("</" + tag + ">");
    return out.join("\n");
  }

  function chatMarkdown(src) {
    if (!src) return "";
    var codeBlocks = [];
    src = src.replace(/```([a-z]*)\n([\s\S]*?)```/g, function (_, lang, body) {
      codeBlocks.push("<pre><code>" + escapeHtml(body) + "</code></pre>");
      return " CB" + (codeBlocks.length - 1) + " ";
    });
    src = escapeHtml(src);
    src = src.replace(/^### (.+)$/gm, "<h3>$1</h3>");
    src = src.replace(/^## (.+)$/gm, "<h2>$1</h2>");
    src = src.replace(/^# (.+)$/gm, "<h1>$1</h1>");
    src = src.replace(/`([^`\n]+)`/g, "<code>$1</code>");
    src = src.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
    src = src.replace(/(?<!\*)\*([^*\n]+)\*(?!\*)/g, "<em>$1</em>");
    src = src.replace(/\[([^\]]+)\]\(([^)]+)\)/g, function (_, text, url) {
      var safe = /^(https?:|\/|\.{0,2}\/)/.test(url) ? url : "#";
      return (
        '<a href="' + safe + '" target="_blank" rel="noopener noreferrer">' + text + "</a>"
      );
    });
    src = chatGroupList(src, "^- (.+)$", "ul");
    src = chatGroupList(src, "^\\d+\\. (.+)$", "ol");
    src = src
      .split(/\n{2,}/)
      .map(function (chunk) {
        var trimmed = chunk.trim();
        if (!trimmed) return "";
        if (/^(<h\d|<ul|<ol|<pre|<p)/.test(trimmed)) return trimmed;
        return "<p>" + trimmed.replace(/\n/g, "<br>") + "</p>";
      })
      .join("\n");
    src = src.replace(/ CB(\d+) /g, function (_, i) {
      return codeBlocks[+i];
    });
    return src;
  }

  // <thc-chat> — the shared conversation view. Streams the agent reply,
  // renders markdown, drops a grey chip for each tool call (so text runs
  // don't merge), and reports turn state via `turnstart`/`turnend` events
  // so a shell's own composer can enable/disable its send button.
  //
  //   <thc-chat agent-label="Research"></thc-chat>
  //   const c = document.querySelector("thc-chat");
  //   c.system("**Welcome.** …");     // seed a system/intro message
  //   c.send("research X");            // append user msg + run the turn
  //   c.addEventListener("turnstart", …); c.addEventListener("turnend", …);
  class ThcChat extends HTMLElement {
    connectedCallback() {
      if (this._init) return;
      this._init = true;
      this._agentLabel = this.getAttribute("agent-label") || "";
      this.innerHTML = '<div class="thc-chat-scroll"></div>';
      this._scroll = this.querySelector(".thc-chat-scroll");
      this._body = null; // streaming agent body element, or null
      this._buffer = "";
      this._wire();
    }

    _wire() {
      var t = bridge();
      if (!t || typeof t.on !== "function") return;
      var self = this;
      t.on("text", function (p) {
        self._onText(p);
      });
      t.on("tool_call", function (p) {
        self._onToolCall(p);
      });
      t.on("tool_result", function (p) {
        self._onToolResult(p);
      });
      t.on("done", function () {
        self._onDone();
      });
      t.on("error", function (p) {
        self._onError(p);
      });
    }

    // ── public API ──
    send(text) {
      text = (text || "").trim();
      if (!text) return false;
      var t = bridge();
      if (!t || typeof t.run !== "function") return false;
      this._append("user", text);
      this._setBusy(true);
      try {
        t.run(text);
      } catch (e) {
        this._onError({ error: String(e) });
      }
      return true;
    }
    system(md) {
      this._append("system", md);
    }
    clear() {
      this._scroll.innerHTML = "";
      this._body = null;
      this._buffer = "";
    }
    get busy() {
      return !!this._busy;
    }

    // ── stream handlers ──
    _onText(p) {
      var chunk = typeof p === "string" ? p : (p && p.text) || "";
      if (!chunk) return;
      if (!this._body) {
        this._body = this._append("agent", "", true);
        this._buffer = "";
      }
      this._buffer += chunk;
      this._body.innerHTML = chatMarkdown(this._buffer);
      this._toBottom();
    }
    _onToolCall(p) {
      // Close the current text run and drop a grey chip between the text
      // before and after, so a tool call never runs paragraphs together.
      this._finalize();
      var chip = document.createElement("div");
      chip.className = "thc-chat-tool";
      chip.textContent = (p && (p.label || p.name)) || "tool";
      this._scroll.appendChild(chip);
      this._toBottom();
    }
    _onToolResult(p) {
      // Output is available at p.output if a shell wants it; v1 keeps the
      // transcript clean and shows only the call chip.
      void p;
    }
    _onDone() {
      this._finalize();
      this._setBusy(false);
    }
    _onError(p) {
      this._finalize();
      this._append("system", "⚠ " + ((p && p.error) || "Turn failed."));
      this._setBusy(false);
    }

    // ── helpers ──
    _finalize() {
      if (this._body) {
        this._body.classList.remove("thc-chat-streaming");
        this._body.innerHTML = chatMarkdown(this._buffer);
      }
      this._body = null;
      this._buffer = "";
    }
    _setBusy(on) {
      this._busy = on;
      this.dispatchEvent(new CustomEvent(on ? "turnstart" : "turnend"));
    }
    _append(kind, md, streaming) {
      var wrap = document.createElement("div");
      wrap.className = "thc-msg thc-msg-" + kind;
      var labelText = kind === "user" ? "You" : kind === "agent" ? this._agentLabel : "";
      if (labelText) {
        var label = document.createElement("div");
        label.className = "thc-msg-label";
        label.textContent = labelText;
        wrap.appendChild(label);
      }
      var body = document.createElement("div");
      body.className = "thc-msg-body" + (streaming ? " thc-chat-streaming" : "");
      body.innerHTML =
        kind === "user" ? escapeHtml(md).replace(/\n/g, "<br>") : chatMarkdown(md);
      wrap.appendChild(body);
      this._scroll.appendChild(wrap);
      this._toBottom();
      return body;
    }
    _toBottom() {
      this._scroll.scrollTop = this._scroll.scrollHeight;
    }
  }

  customElements.define("thc-chat", ThcChat);
})();
