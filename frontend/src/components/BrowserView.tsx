import { useEffect, useRef, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

// docs/browser Phase 1 — the Browser tab for the engine-managed
// Playwright MCP browser (`browserEnabled` in settings.json).
//
// Layout: main column = status card + live screenshot + activity feed;
// right sidebar = a compact chat so the user can direct the agent
// ("take over and fill this form") without leaving the tab — the
// 12gram split-screen workflow in one place.
//
//   - status        ← `browser_status_get` IPC
//   - screenshot    ← `browser_screenshot_get` IPC: runs directly on
//                     the managed MCP client (no agent loop, no
//                     tokens), auto-captured ~1s after each browser
//                     tool result while the tab is visible
//   - activity      ← the same `chat_tool_call`/`chat_tool_result`
//                     dispatches Chat renders, filtered to `browser__*`
//   - sidebar chat  ← `shell_input` (same pipe as the Chat tab) +
//                     `chat_user_message` / `chat_text_delta` events

type BrowserStatus = {
  enabled: boolean;
  headless: boolean;
  command: string;
  command_found: boolean;
  cdp: boolean;
};

type ActivityEntry = {
  id: number;
  at: string;
  kind: "call" | "result" | "console";
  tool: string;
  detail: string;
};

type ChatMsg = {
  id: number;
  role: "user" | "assistant" | "system";
  text: string;
};

const MAX_ENTRIES = 200;
const MAX_CHAT = 80;
const SHOT_DEBOUNCE_MS = 1000;

function shorten(s: string, n: number): string {
  return s.length > n ? s.slice(0, n) + "…" : s;
}

export function BrowserView({ active }: { active: boolean }) {
  const [status, setStatus] = useState<BrowserStatus | null>(null);
  const [entries, setEntries] = useState<ActivityEntry[]>([]);
  const [shot, setShot] = useState<{ src: string; at: string } | null>(null);
  const [shotErr, setShotErr] = useState<string>("");
  const [shotBusy, setShotBusy] = useState(false);
  const [chat, setChat] = useState<ChatMsg[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [busy, setBusy] = useState(false);
  // Interactive takeover (Phase 2 slice 2): when on, the screenshot is
  // clickable/typeable — every action routes through the allowlisted
  // `browser_input_call` arm and refreshes the screenshot.
  const [takeover, setTakeover] = useState(false);
  // slice 3: live CDP screencast — frames stream in while takeover is
  // on and the engine owns the browser; falls back to screenshots.
  const [live, setLive] = useState(false);
  const [pageUrl, setPageUrl] = useState("");
  const [urlInput, setUrlInput] = useState("");
  const [typeInput, setTypeInput] = useState("");
  const [inputErr, setInputErr] = useState("");

  const nextId = useRef(1);
  const listRef = useRef<HTMLDivElement | null>(null);
  const chatRef = useRef<HTMLDivElement | null>(null);
  const activeRef = useRef(active);
  const shotTimer = useRef<number | null>(null);
  // Browser activity that happened while the tab was hidden — capture
  // one fresh screenshot when the user comes back.
  const staleShot = useRef(false);

  activeRef.current = active;

  function requestShot() {
    setShotBusy(true);
    send({ type: "browser_screenshot_get" });
  }

  function sendInput(tool: string, args: Record<string, unknown>) {
    setInputErr("");
    send({ type: "browser_input_call", tool, args });
  }

  // Map a click on the rendered screenshot to page coordinates. The
  // <img> uses object-contain, so the drawn picture may be letterboxed
  // inside the element box — account for that before scaling to the
  // image's natural (viewport) size.
  function imgClickCoords(e: React.MouseEvent<HTMLImageElement>) {
    const img = e.currentTarget;
    const rect = img.getBoundingClientRect();
    const natW = img.naturalWidth || 1;
    const natH = img.naturalHeight || 1;
    const scale = Math.min(rect.width / natW, rect.height / natH);
    const drawnW = natW * scale;
    const drawnH = natH * scale;
    const offX = (rect.width - drawnW) / 2;
    const offY = (rect.height - drawnH) / 2;
    const x = (e.clientX - rect.left - offX) / scale;
    const y = (e.clientY - rect.top - offY) / scale;
    if (x < 0 || y < 0 || x > natW || y > natH) return null;
    return { x: Math.round(x), y: Math.round(y) };
  }

  function onShotClick(e: React.MouseEvent<HTMLImageElement>) {
    if (!takeover) return;
    const pt = imgClickCoords(e);
    if (!pt) return;
    if (liveRef.current) {
      send({ type: "browser_cdp_input", kind: "click", args: { x: pt.x, y: pt.y } });
      return;
    }
    sendInput("browser_mouse_click_xy", {
      element: "user takeover click",
      x: pt.x,
      y: pt.y,
    });
  }

  function onShotMouseMove(e: React.MouseEvent<HTMLImageElement>) {
    if (!takeoverRef.current) return;
    const pt = imgClickCoords(e);
    if (pt) hoverPos.current = pt;
  }

  // Wheel → remote scroll, throttled by accumulating deltas. Attached
  // via ref with passive:false so the local pane doesn't also scroll.
  const wheelAcc = useRef({ x: 0, y: 0, timer: null as number | null });
  const takeoverRef = useRef(takeover);
  takeoverRef.current = takeover;
  const liveRef = useRef(live);
  liveRef.current = live;
  const statusRef = useRef<BrowserStatus | null>(null);
  statusRef.current = status;
  // Last pointer position over the page image (page coordinates) —
  // CDP wheel events want x/y context.
  const hoverPos = useRef({ x: 0, y: 0 });
  const shotImgRef = useRef<HTMLImageElement | null>(null);
  useEffect(() => {
    const img = shotImgRef.current;
    if (!img) return;
    const onWheel = (e: WheelEvent) => {
      if (!takeoverRef.current) return;
      e.preventDefault();
      wheelAcc.current.x += e.deltaX;
      wheelAcc.current.y += e.deltaY;
      if (wheelAcc.current.timer === null) {
        wheelAcc.current.timer = window.setTimeout(() => {
          const { x, y } = wheelAcc.current;
          wheelAcc.current = { x: 0, y: 0, timer: null };
          sendInput("browser_mouse_wheel", {
            deltaX: Math.round(x),
            deltaY: Math.round(y),
          });
        }, 150);
      }
    };
    img.addEventListener("wheel", onWheel, { passive: false });
    return () => img.removeEventListener("wheel", onWheel);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shot !== null]);

  function scheduleShot() {
    if (liveRef.current) return; // screencast frames already flow
    if (!activeRef.current) {
      staleShot.current = true;
      return;
    }
    if (shotTimer.current !== null) window.clearTimeout(shotTimer.current);
    shotTimer.current = window.setTimeout(() => {
      shotTimer.current = null;
      requestShot();
    }, SHOT_DEBOUNCE_MS);
  }

  useEffect(() => {
    const unsub = subscribe((msg: any) => {
      if (msg.type === "browser_status") {
        setStatus({
          enabled: Boolean(msg.enabled),
          headless: Boolean(msg.headless),
          command: typeof msg.command === "string" ? msg.command : "",
          command_found: Boolean(msg.command_found),
          cdp: Boolean(msg.cdp),
        });
        return;
      }
      if (msg.type === "browser_screenshot") {
        setShotBusy(false);
        if (msg.ok && typeof msg.data === "string") {
          const mime = typeof msg.mime === "string" ? msg.mime : "image/png";
          setShot({
            src: `data:${mime};base64,${msg.data}`,
            at: new Date().toLocaleTimeString([], { hour12: false }),
          });
          setShotErr("");
          staleShot.current = false;
        } else {
          setShotErr(typeof msg.error === "string" ? msg.error : "capture failed");
        }
        return;
      }
      if (msg.type === "browser_frame" && typeof msg.data === "string") {
        setShot({
          src: `data:image/jpeg;base64,${msg.data}`,
          at: new Date().toLocaleTimeString([], { hour12: false }),
        });
        return;
      }
      if (msg.type === "browser_screencast") {
        setLive(Boolean(msg.active));
        if (!msg.ok && typeof msg.error === "string") setInputErr(msg.error);
        return;
      }
      if (msg.type === "browser_console" && typeof msg.text === "string") {
        const level = typeof msg.level === "string" ? msg.level : "log";
        if (level === "error" || level === "warning") {
          push("console", level, shorten(msg.text, 300));
        }
        return;
      }
      if (msg.type === "browser_nav" && typeof msg.url === "string") {
        setPageUrl(msg.url);
        return;
      }
      if (msg.type === "browser_input_result") {
        if (msg.ok) {
          // The page just changed under user input — refresh promptly.
          scheduleShot();
        } else if (typeof msg.error === "string") {
          setInputErr(msg.error);
        }
        return;
      }
      if (msg.type === "gui_busy_changed") {
        setBusy(Boolean(msg.busy));
        return;
      }
      // Sidebar chat transcript — the SAME shared conversation the
      // Chat + Terminal tabs render. Session-level events (slash
      // output, /clear, /load, /new) keep all three views in sync.
      if (msg.type === "chat_user_message" && typeof msg.text === "string") {
        pushChat("user", msg.text);
        return;
      }
      if (msg.type === "chat_text_delta" && typeof msg.text === "string") {
        appendAssistant(msg.text);
        return;
      }
      if (msg.type === "chat_slash_output" && typeof msg.text === "string") {
        pushChat("system", msg.text);
        return;
      }
      if (msg.type === "chat_error" && typeof msg.text === "string") {
        pushChat("system", `⚠ ${msg.text}`);
        return;
      }
      if (msg.type === "new_session_ack") {
        setChat([]);
        return;
      }
      if (msg.type === "chat_history_replaced") {
        const restored: ChatMsg[] = [];
        if (Array.isArray(msg.messages)) {
          for (const m of msg.messages as { role: string; content: string }[]) {
            if (typeof m.content !== "string" || !m.content) continue;
            if (m.role === "user") restored.push({ id: nextId.current++, role: "user", text: m.content });
            else if (m.role === "assistant") restored.push({ id: nextId.current++, role: "assistant", text: m.content });
            // tool/system entries stay in the full Chat tab; the
            // sidebar keeps the compact user/assistant thread.
          }
        }
        setChat(restored.slice(-MAX_CHAT));
        return;
      }
      if (msg.type === "chat_done") {
        setBusy(false);
        return;
      }
      // Activity: MCP tools register as `browser__<tool>`.
      if (msg.type === "chat_tool_call") {
        const raw = typeof msg.tool_name === "string" ? msg.tool_name : "";
        if (!raw.startsWith("browser__")) return;
        const detail = msg.input ? shorten(JSON.stringify(msg.input), 300) : "";
        push("call", raw.slice("browser__".length), detail);
      } else if (msg.type === "chat_tool_result") {
        const raw = typeof msg.name === "string" ? msg.name : "";
        if (!raw.startsWith("browser__")) return;
        const detail = typeof msg.output === "string" ? shorten(msg.output, 300) : "";
        push("result", raw.slice("browser__".length), detail);
        // The page just (probably) changed — refresh the screenshot.
        scheduleShot();
      }
    });
    send({ type: "browser_status_get" });
    return unsub;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function push(kind: ActivityEntry["kind"], tool: string, detail: string) {
    const at = new Date().toLocaleTimeString([], { hour12: false });
    setEntries((prev) => {
      const next = [...prev, { id: nextId.current++, at, kind, tool, detail }];
      return next.length > MAX_ENTRIES ? next.slice(next.length - MAX_ENTRIES) : next;
    });
  }

  function pushChat(role: ChatMsg["role"], text: string) {
    setChat((prev) => {
      const next = [...prev, { id: nextId.current++, role, text }];
      return next.length > MAX_CHAT ? next.slice(next.length - MAX_CHAT) : next;
    });
  }

  function appendAssistant(delta: string) {
    setChat((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.role === "assistant") {
        const next = prev.slice(0, -1);
        next.push({ ...last, text: last.text + delta });
        return next;
      }
      const next = [...prev, { id: nextId.current++, role: "assistant" as const, text: delta }];
      return next.length > MAX_CHAT ? next.slice(next.length - MAX_CHAT) : next;
    });
  }

  function sendChat() {
    const text = chatInput.trim();
    if (!text) return;
    setChatInput("");
    setBusy(true);
    send({ type: "shell_input", text, attachments: [] });
  }

  // Catch up on a screenshot missed while the tab was hidden.
  useEffect(() => {
    if (active && staleShot.current) requestShot();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active]);

  // slice 3: screencast lifecycle — run while takeover is on, the tab
  // is visible, and the engine owns the browser (status.cdp).
  useEffect(() => {
    const want = takeover && active && Boolean(status?.cdp);
    if (want && !live) {
      send({ type: "browser_screencast_start" });
    } else if (!want && live) {
      send({ type: "browser_screencast_stop" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [takeover, active, status?.cdp]);

  useEffect(() => {
    if (active && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
  }, [entries, active]);

  useEffect(() => {
    if (active && chatRef.current) {
      chatRef.current.scrollTop = chatRef.current.scrollHeight;
    }
  }, [chat, busy, active]);

  const browserUsed = entries.length > 0;

  return (
    <div className="h-full flex gap-3 p-4 overflow-hidden">
      {/* ── Main column: status + screenshot + activity ── */}
      <div className="flex-1 min-w-0 flex flex-col gap-3 overflow-hidden">
        <div
          className="rounded-lg border p-3 shrink-0"
          style={{ borderColor: "var(--border)", background: "var(--bg-secondary)" }}
        >
          <div className="flex items-center gap-2 mb-1">
            <span className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
              Managed browser
            </span>
            {status && (
              <span
                className="text-[10px] px-2 py-0.5 rounded-full font-medium"
                style={{
                  background: status.enabled ? "var(--accent)" : "var(--bg-primary)",
                  color: status.enabled ? "white" : "var(--text-secondary)",
                  border: status.enabled ? "none" : "1px solid var(--border)",
                }}
              >
                {status.enabled ? (status.headless ? "headless" : "headed") : "disabled"}
              </span>
            )}
            <div className="flex-1" />
            {status?.enabled && (
              <button
                onClick={() => setTakeover((t) => !t)}
                className="text-[11px] px-2 py-0.5 rounded border font-medium"
                style={{
                  borderColor: takeover ? "var(--accent)" : "var(--border)",
                  color: takeover ? "white" : "var(--text-secondary)",
                  background: takeover ? "var(--accent)" : "transparent",
                }}
                title="Interact with the page directly — click, type, and scroll on the screenshot"
              >
                🖱 {takeover ? "Taking over" : "Take over"}
              </button>
            )}
            {status?.enabled && (
              <button
                onClick={requestShot}
                disabled={shotBusy}
                className="text-[11px] px-2 py-0.5 rounded border"
                style={{
                  borderColor: "var(--border)",
                  color: "var(--text-secondary)",
                  opacity: shotBusy ? 0.5 : 1,
                }}
                title="Capture a screenshot of the managed browser now"
              >
                {shotBusy ? "capturing…" : "📷 capture"}
              </button>
            )}
          </div>
          {!status && (
            <p className="text-xs" style={{ color: "var(--text-secondary)" }}>Loading…</p>
          )}
          {status && !status.enabled && (
            <p className="text-xs leading-relaxed" style={{ color: "var(--text-secondary)" }}>
              Browser automation is off. Set <code>&quot;browserEnabled&quot;: true</code> in{" "}
              <code>.thclaws/settings.json</code> and reload — the engine then manages the
              official Playwright MCP server and the agent gains <code>browser_*</code> tools.
            </p>
          )}
          {status && status.enabled && (
            <>
              <p className="text-xs font-mono" style={{ color: "var(--text-secondary)" }}>
                {status.command}
              </p>
              {!status.command_found && (
                <p className="text-xs mt-1 leading-relaxed" style={{ color: "#dc2626" }}>
                  ⚠ the browser server&apos;s command isn&apos;t on PATH — it can&apos;t start.
                  On desktop, install Node.js (e.g. <code>brew install node</code>) and
                  restart thClaws.
                </p>
              )}
            </>
          )}
        </div>

        {/* Screenshot panel — the in-tab view of the page. */}
        {status?.enabled && (
          <div
            className="rounded-lg border shrink-0 overflow-hidden"
            style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
          >
            {shot ? (
              <div>
                <img
                  ref={shotImgRef}
                  src={shot.src}
                  alt="Latest browser screenshot"
                  className="w-full max-h-[45vh] object-contain select-none"
                  style={{
                    background: "#fff",
                    cursor: takeover ? "crosshair" : "default",
                    outline: takeover ? "2px solid var(--accent)" : "none",
                    outlineOffset: -2,
                  }}
                  onClick={onShotClick}
                  onMouseMove={onShotMouseMove}
                  draggable={false}
                />
                <div
                  className="text-[10px] px-2 py-1 flex justify-between"
                  style={{ color: "var(--text-secondary)", borderTop: "1px solid var(--border)" }}
                >
                  <span>
                    {takeover
                      ? live
                        ? `● LIVE — click / scroll / type${pageUrl ? ` · ${shorten(pageUrl, 60)}` : ""}`
                        : "takeover: click / scroll on the page, type below"
                      : "auto-captured after browser actions"}
                  </span>
                  <span>{shot.at}</span>
                </div>
              </div>
            ) : (
              <div className="p-3 text-xs" style={{ color: "var(--text-secondary)" }}>
                {shotErr
                  ? `Screenshot: ${shotErr}`
                  : browserUsed
                    ? "Capturing…"
                    : takeover
                      ? "Enter a URL below to start browsing."
                      : "The page preview appears here after the agent's first browser action."}
              </div>
            )}
            {takeover && (
              <div
                className="p-2 flex flex-col gap-1.5"
                style={{ borderTop: "1px solid var(--border)" }}
              >
                <div className="flex gap-1.5">
                  <button
                    onClick={() => sendInput("browser_navigate_back", {})}
                    className="text-[11px] px-2 rounded border"
                    style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                    title="Back"
                  >
                    ←
                  </button>
                  <input
                    value={urlInput}
                    onChange={(e) => setUrlInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && urlInput.trim()) {
                        const u = urlInput.trim();
                        sendInput("browser_navigate", {
                          url: /^[a-z]+:\/\//i.test(u) ? u : `https://${u}`,
                        });
                      }
                    }}
                    placeholder="Go to URL… (Enter)"
                    className="flex-1 min-w-0 text-[11px] px-2 py-1 rounded border outline-none font-mono"
                    style={{
                      borderColor: "var(--border)",
                      background: "var(--bg-secondary)",
                      color: "var(--text-primary)",
                    }}
                  />
                </div>
                <div className="flex gap-1.5 items-center">
                  <input
                    value={typeInput}
                    onChange={(e) => setTypeInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && typeInput) {
                        if (live) {
                          send({ type: "browser_cdp_input", kind: "text", args: { text: typeInput } });
                        } else {
                          sendInput("type_text", { text: typeInput });
                        }
                        setTypeInput("");
                      }
                    }}
                    placeholder="Type into the focused field… (Enter sends)"
                    className="flex-1 min-w-0 text-[11px] px-2 py-1 rounded border outline-none"
                    style={{
                      borderColor: "var(--border)",
                      background: "var(--bg-secondary)",
                      color: "var(--text-primary)",
                    }}
                  />
                  {["Enter", "Tab", "Escape", "Backspace"].map((k) => (
                    <button
                      key={k}
                      onClick={() =>
                        live
                          ? send({ type: "browser_cdp_input", kind: "key", args: { key: k } })
                          : sendInput("browser_press_key", { key: k })
                      }
                      className="text-[10px] px-1.5 py-1 rounded border font-mono"
                      style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                      title={`Press ${k}`}
                    >
                      {k === "Escape" ? "Esc" : k === "Backspace" ? "⌫" : k}
                    </button>
                  ))}
                </div>
                {inputErr && (
                  <div className="text-[10px]" style={{ color: "#dc2626" }}>
                    {inputErr}
                  </div>
                )}
              </div>
            )}
          </div>
        )}

        <div className="text-xs font-semibold shrink-0" style={{ color: "var(--text-secondary)" }}>
          Activity {entries.length > 0 && `(${entries.length})`}
        </div>
        <div
          ref={listRef}
          className="flex-1 min-h-0 overflow-y-auto rounded-lg border p-2 font-mono text-[11px] leading-relaxed"
          style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
        >
          {entries.length === 0 ? (
            <div className="p-2" style={{ color: "var(--text-secondary)" }}>
              No browser activity yet. Ask the agent (here in the sidebar →) something like
              “open example.com and summarize the page”.
            </div>
          ) : (
            entries.map((e) => (
              <div key={e.id} className="px-1 py-0.5 flex gap-2 items-baseline">
                <span style={{ color: "var(--text-secondary)" }}>{e.at}</span>
                <span
                  className="shrink-0"
                  style={{
                    color:
                      e.kind === "call"
                        ? "var(--accent)"
                        : e.kind === "console"
                          ? e.tool === "error"
                            ? "#dc2626"
                            : "#d97706"
                          : "var(--text-secondary)",
                  }}
                >
                  {e.kind === "call" ? "→" : e.kind === "console" ? "◆" : "←"}
                </span>
                <span className="shrink-0 font-semibold" style={{ color: "var(--text-primary)" }}>
                  {e.tool}
                </span>
                <span className="break-all" style={{ color: "var(--text-secondary)" }}>
                  {e.detail}
                </span>
              </div>
            ))
          )}
        </div>
      </div>

      {/* ── Chat sidebar — direct the agent without leaving the tab ── */}
      <div
        className="w-[320px] shrink-0 flex flex-col rounded-lg border overflow-hidden"
        style={{ borderColor: "var(--border)", background: "var(--bg-secondary)" }}
      >
        <div
          className="px-3 py-2 text-xs font-semibold flex items-center gap-2 shrink-0"
          style={{ color: "var(--text-primary)", borderBottom: "1px solid var(--border)" }}
        >
          Agent
          {busy && (
            <span className="text-[10px] font-normal" style={{ color: "var(--accent)" }}>
              ● working…
            </span>
          )}
        </div>
        <div ref={chatRef} className="flex-1 min-h-0 overflow-y-auto p-2 flex flex-col gap-2">
          {chat.length === 0 && (
            <div className="text-[11px] p-1 leading-relaxed" style={{ color: "var(--text-secondary)" }}>
              Same conversation as the Chat tab. Tell the agent what to do in the
              browser — “log in is done, take over and export the report”.
            </div>
          )}
          {chat.map((m) => (
            <div
              key={m.id}
              className="rounded-md px-2 py-1.5 text-[12px] leading-relaxed whitespace-pre-wrap break-words"
              style={
                m.role === "user"
                  ? { background: "var(--accent)", color: "white", alignSelf: "flex-end", maxWidth: "92%" }
                  : m.role === "system"
                    ? { background: "transparent", color: "var(--text-secondary)", alignSelf: "stretch", maxWidth: "100%", fontFamily: "ui-monospace, monospace", fontSize: 11, whiteSpace: "pre-wrap" }
                    : { background: "var(--bg-primary)", color: "var(--text-primary)", alignSelf: "flex-start", maxWidth: "92%", border: "1px solid var(--border)" }
              }
            >
              {m.text}
            </div>
          ))}
        </div>
        <div
          className="p-2 flex gap-1.5 shrink-0"
          style={{ borderTop: "1px solid var(--border)" }}
        >
          <input
            value={chatInput}
            onChange={(e) => setChatInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                sendChat();
              }
            }}
            placeholder="Direct the agent…"
            className="flex-1 min-w-0 text-[12px] px-2 py-1.5 rounded border outline-none"
            style={{
              borderColor: "var(--border)",
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
            }}
          />
          <button
            onClick={sendChat}
            className="text-[12px] px-2.5 rounded font-medium"
            style={{ background: "var(--accent)", color: "white" }}
          >
            →
          </button>
        </div>
      </div>
    </div>
  );
}
