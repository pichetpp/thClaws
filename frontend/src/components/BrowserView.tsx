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
};

type ActivityEntry = {
  id: number;
  at: string;
  kind: "call" | "result";
  tool: string;
  detail: string;
};

type ChatMsg = {
  id: number;
  role: "user" | "assistant";
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

  function scheduleShot() {
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
      if (msg.type === "gui_busy_changed") {
        setBusy(Boolean(msg.busy));
        return;
      }
      // Sidebar chat transcript — same events the Chat tab renders.
      if (msg.type === "chat_user_message" && typeof msg.text === "string") {
        pushChat("user", msg.text);
        return;
      }
      if (msg.type === "chat_text_delta" && typeof msg.text === "string") {
        appendAssistant(msg.text);
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

  function push(kind: "call" | "result", tool: string, detail: string) {
    const at = new Date().toLocaleTimeString([], { hour12: false });
    setEntries((prev) => {
      const next = [...prev, { id: nextId.current++, at, kind, tool, detail }];
      return next.length > MAX_ENTRIES ? next.slice(next.length - MAX_ENTRIES) : next;
    });
  }

  function pushChat(role: "user" | "assistant", text: string) {
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
                  src={shot.src}
                  alt="Latest browser screenshot"
                  className="w-full max-h-[45vh] object-contain"
                  style={{ background: "#fff" }}
                />
                <div
                  className="text-[10px] px-2 py-1 flex justify-between"
                  style={{ color: "var(--text-secondary)", borderTop: "1px solid var(--border)" }}
                >
                  <span>auto-captured after browser actions</span>
                  <span>{shot.at}</span>
                </div>
              </div>
            ) : (
              <div className="p-3 text-xs" style={{ color: "var(--text-secondary)" }}>
                {shotErr
                  ? `Screenshot: ${shotErr}`
                  : browserUsed
                    ? "Capturing…"
                    : "The page preview appears here after the agent's first browser action."}
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
                  style={{ color: e.kind === "call" ? "var(--accent)" : "var(--text-secondary)" }}
                >
                  {e.kind === "call" ? "→" : "←"}
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
