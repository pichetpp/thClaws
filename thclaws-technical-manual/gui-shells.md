# GUI Shells — the `window.thclaws.*` bridge

How a marketplace / catalog agent ships its own HTML+JS UI ("GUI shell") and how that UI talks to the engine. A shell is a folder of static assets (`index.html` / `main.js` / `manifest.json`) that the engine serves inside an iframe and injects a single global into: `window.thclaws`. The bridge is the **only** capability surface — a shell has no direct filesystem, no network beyond declared hosts, and no access to another shell's storage.

This doc covers: the two transports (Mode A desktop iframe vs Mode B standalone `--serve`), bridge injection, the request/reply + event wire format, the host-side IPC handlers, per-shell storage, the full method surface and **what is actually wired vs. on-the-object-but-stubbed**, theme / full-screen integration, the permission model, and the preview mock.

Related: [`app-architecture.md`](app-architecture.md) (the underlying Rust↔JS IPC bridge this rides on), [`mcp.md`](mcp.md) (MCP-Apps widgets — a different host↔widget postMessage protocol), [`serve-mode.md`](serve-mode.md) + [`multi-tenant-serve.md`](multi-tenant-serve.md) (Mode B / per-user storage roots), [`built-in-tools.md`](built-in-tools.md) (the Media Studio shell + media-tool gating).

## 1. Module layout

| File | Role |
|---|---|
| `crates/core/assets/gui-shell-bridge.js` | The bridge runtime injected into every shell. Builds `window.thclaws`, marshals JSON to the host, fans events out to subscribers. Embedded via `gui_shell/mod.rs:29` `BRIDGE_RUNTIME = include_str!(…)`. |
| `crates/core/src/gui_shell/mod.rs` | Module root; serves the bridge at `thclaws://localhost/gui-shell-bridge.js`. |
| `crates/core/src/gui_shell/{manifest,registry,router,serve,storage,tokens,shell_cli,shell_preview}.rs` | Manifest schema, installed-shell registry, URL routing, Mode B serve handler, per-shell storage backend, serve-token mint/verify, `thclaws shell …` CLI, and the preview mock. |
| `crates/core/src/ipc.rs` | Host-side `gui_shell_*` dispatch arms (shared GUI + `--serve`). |
| `frontend/src/components/UIView.tsx` | Mode A only: the React iframe host that marshals between the shell's `postMessage` and the backend `window.ipc` bridge. |
| `frontend/src/App.tsx` | Handles parent-only signals (`hotkey` / `ui`) the iframe forwards (`App.tsx:539`, `ns === "thclaws-shell"`). |

## 2. Two transports

`thclaws.transport` is `"tauri"` (Mode A) or `"ws"` (Mode B). The bridge picks the mode from `window.__thclaws_shell_mode` (the Mode B serve handler sets it to `"ws"` before the bridge script runs).

- **Mode A — desktop iframe (`transport: "tauri"`).** URL `thclaws://localhost/gui-shell/<id>/<path>?session=<sid>`. The `thclaws://` protocol handler (`gui.rs:852`, `req_path == "/gui-shell-bridge.js"`) serves the bridge and the shell assets; the bridge tag is injected into `<head>` at serve time by the inject helper at `gui.rs:468`. The shell `postMessage`s to its parent (the React `UIView` iframe host), which relays to/from the Rust backend over the existing `window.ipc` / `__thclaws_dispatch` bridge.
- **Mode B — standalone serve (`transport: "ws"`).** URL `/t/<token>/<path>?session=<sid>` under `thclaws --serve --gui-shell`. There is no parent React app: the bridge opens a WebSocket directly to the engine (`window.__thclaws_shell_ws_url`, default `/__ws`), opened lazily on first send with exponential backoff reconnection (500 ms → 30 s cap). Identity (`shellId`, `sessionId`) is injected as `window.__thclaws_shell_id` / `__thclaws_shell_session_id` at HTML render time because the `/t/<token>/` URL carries neither. The cloud `--serve`-over-https case nests the iframe under the same Traefik-stripped prefix as the parent workspace URL.

Both modes converge on one `handleShellEvent(data)` fan-out in the bridge, so shell code is transport-agnostic.

## 3. Wire format

The bridge's `send(type, payload)` allocates a `requestId`, stores `{resolve, reject}` in a `pending` map, and emits a frame:

**Request (shell → host).**
- Mode A `postMessage` to parent: `{ ns: "thclaws-shell", requestId, type, payload, shellId, sessionId }`. `UIView.tsx:71` forwards it to the backend as `{ type: "gui_shell_<type>", id: requestId, sessionId, shellId, ...payload }` (`UIView.tsx:95`).
- Mode B WS frame: identical `{ type: "gui_shell_<type>", id: requestId, sessionId, shellId, ...payload }`.

**Reply (host → shell).** The host dispatches a `gui_shell_event` envelope correlated by `replyTo`:

```json
{ "type": "gui_shell_event", "sessionId": "<sid>", "replyTo": 7, "result": <any> }
{ "type": "gui_shell_event", "sessionId": "<sid>", "replyTo": 7, "error": "<msg>" }
```

`handleShellEvent` matches `replyTo` against `pending` and resolves/rejects the promise. In Mode A, `UIView.tsx:122` subscribes to backend dispatches and re-posts any `gui_shell_event` into the iframe as `{ ns: "thclaws-shell-event", ... }`; in Mode B the bridge reads it straight off the WS.

**Events (host → shell, unsolicited).** Same `gui_shell_event` envelope but carrying `event` + `payload` instead of `replyTo`. The bridge fans these out to `thclaws.on(event, …)` subscribers. Event names: `ready`, `text`, `done`, `error`, `tool_call`, `tool_result`, `fullscreen`, `theme`.

> **Tier 1 has no per-tab session filtering** (`UIView.tsx:124`): one shared session, so every active shell tab receives every event. Per-tab `sessionId` filtering is Tier 2.

## 4. Host-side handlers (`ipc.rs`)

`handle_ipc` returns `bool` (the [`running-modes.md`](running-modes.md) invariant); the `gui_shell_*` arms are shared by GUI and `--serve`. Implemented arms:

| Backend type | `ipc.rs` | Bridge method |
|---|---|---|
| `gui_shell_run` | `:275` | `thclaws.run(prompt, opts?)` |
| `gui_shell_cancel` | `:304` | `thclaws.cancel(runId?)` (fire-and-forget, no reply) |
| `gui_shell_tool_invoke` | `:334` | `thclaws.callTool(name, args)` / `thclaws.tools.invoke(name, args)` |
| `gui_shell_storage_get` | `:425` | `thclaws.storage.get(key)` |
| `gui_shell_storage_set` | `:523` | `thclaws.storage.set(key, value)` |
| `gui_shell_list` | `:488` | *(frontend shell-list, not a bridge method)* |

**`gui_shell_tool_invoke` gating** (`ipc.rs:334`–`:420`): read-only tools (`ls` / `read` / `glob` / `grep` / `web_fetch` / `web_search` / `kms_read` / … ) run directly; mutating tools (`Bash` / `Write` / `Edit` / …) reject with "requires approval" (the inline-approval flow is Tier 3, see §6). MCP-contributed tools are **not** reachable in the Tier-2 IPC arm — it builds a fresh built-ins-only `ToolRegistry`; Tier 3 routes through the worker's registry. Media tools are force-enabled when `shell_id == "media-studio"` regardless of `mediaToolsEnabled` (`ipc.rs:359`), making the built-in Media Studio shell a zero-config on-ramp — see [`built-in-tools.md`](built-in-tools.md).

## 5. Storage

Per-shell, per-session JSON, namespaced by shell id so two shells can't read each other's state even on a shared session. Default path `~/.config/thclaws/gui-shell/<shellId>/state/<sessionId>.json` (`ipc.rs:422`, atomic per-set). Under multi-tenant `--serve`, the `SessionRoots` override relocates it into the per-user subtree `<project>/.thclaws/users/<id>/storage/…` — see [`multi-tenant-serve.md`](multi-tenant-serve.md). There is no `delete` handler; the documented delete idiom is `storage.set(key, null)`.

## 6. Method surface — wired vs. stubbed

The bridge object advertises more than the engine currently backs. **Wired end-to-end** (a host handler exists, or it is pure client-side):

- `thclaws.shell.{id,sessionId}`, `thclaws.transport` — identity (client-side).
- `thclaws.run(prompt, opts?) → Promise<{runId}>`, `thclaws.cancel(runId?)`.
- `thclaws.on(event, cb) → unsubscribe`; `thclaws.streamTurn(prompt, opts?)` — async-iterable sugar over `run()` + `on("text"|"done"|"error")` (pure client-side composition).
- `thclaws.callTool(name, args)` / `thclaws.tools.invoke(name, args)` → `gui_shell_tool_invoke`.
- `thclaws.storage.get/set` → `gui_shell_storage_get/set`.
- `thclaws.fileUrl(path) → string|null` — pure client-side path→URL mapping. Mode B accepts a path relative to the shell's project root (`/t/<token>/file-asset/…`); Mode A requires an absolute path (`thclaws://localhost/file-asset/…`), else `null`.
- `thclaws.ui.*` — `theme`, `isFullscreen`, `onTheme`, `onFullscreen`, `exitFullscreen`, `claimExitControl` (see §7).

**On the object but NOT wired host-side** — these call `send()` with a `gui_shell_*` type that has no `ipc.rs` arm, so in production they hang or reject; do not depend on them:

- `thclaws.storage.delete(key)` → `gui_shell_storage_delete` (no arm — use `storage.set(key, null)`).
- `thclaws.permissions.list()` / `.has(action)` → `gui_shell_permissions_list` (no arm).
- `thclaws.awaitApproval(request)` → `gui_shell_await_approval` (no arm; falls back to the system approval modal).
- `thclaws.uploadFile(blob, name?)` → `gui_shell_upload_file` (no arm; use `fileUrl()` on an agent-written file).

The bridge's `awaitApproval`/`uploadFile` `.catch` only rewrites errors containing `"doesn't implement"`, which is emitted **only** by the preview mock (`gui_shell/shell_preview.rs:304`) — under the real engine these settle via the generic error/timeout path, not that message.

## 7. Theme & full-screen integration

The host pushes its resolved state as events the bridge intercepts in `handleShellEvent` before fanning out:

- `theme` (`{mode: "light"|"dark"}`) — the bridge sets `thclaws.ui.theme`, plus `data-theme` + `color-scheme` on `<html>`, so a shell can theme in **CSS alone** (`:root[data-theme="light"]{…}`) with no JS. `thclaws.ui.onTheme(cb)` is for JS-driven styling (canvas/charts) and fires immediately with the current value.
- `fullscreen` (`{active}`) — updates `thclaws.ui.isFullscreen`; `onFullscreen(cb)` fires immediately + on change.

`thclaws.ui.exitFullscreen()` and `claimExitControl()` post **parent-only** envelopes (`type: "hotkey" | "ui"`) that `App.tsx` handles on the window; both are no-ops in Mode B (the standalone shell owns the whole page). `UIView.tsx` replays the current fullscreen + theme to a freshly-loaded shell on the `ready` signal (`UIView.tsx:80`) so subscribers added before the first host event still get an initial value.

## 8. Permission model

`manifest.json::permissions` declares what a shell may do; the user sees the list before install, and anything undeclared throws at call time. The bridge is the only API — no workspace FS, no network beyond declared hosts (CSP injected at serve time).

| Permission | Allows |
|---|---|
| `agent.run` | `thclaws.run()` + event subscription |
| `tools.invoke:<name>` | `thclaws.callTool("<name>", …)` / `tools.invoke(…)` per tool |
| `session.read` / `session.list` | read sidecar session data |
| `fs.shell-scoped` | read/write inside the shell's resolved root |
| `network.outbound:<host>` | `fetch()` to that host |

Publish-safety: `cloud/pack.rs` strips `.thclaws/sessions/`, KMS data, and browser-profile cookies so a shell's local state never leaks into a catalog tarball ([`thclaws-cloud-client.md`](thclaws-cloud-client.md)).

## 9. Preview & doctor

`thclaws shell doctor <dir>` validates the manifest, entry file, permission sanity, and flags Tauri-only APIs that would break in Mode B. `gui_shell/shell_preview.rs` provides a mock `window.thclaws` for design-time preview that replies to unimplemented methods with `"preview mock doesn't implement '<method>'"` — the sentinel the bridge's stub `.catch` arms look for.

## 10. Known gaps

- Tier-2 IPC `tool_invoke` can't reach MCP tools (built-ins-only registry); Tier 3 is the fix.
- No per-tab session filtering in Tier 1 — every shell tab gets every event.
- `storage.delete`, `permissions.list/has`, `awaitApproval`, `uploadFile` are contract stubs without host handlers (§6).
- Inline approval (`awaitApproval`) falls back to the full-screen system modal until wired.
