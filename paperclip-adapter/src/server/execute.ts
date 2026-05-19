/**
 * Drive a thClaws subprocess via its OpenAI Chat Completions API.
 *
 * v2 (dev-plan/21): instead of spawning `thclaws -p prompt` per turn
 * and parsing plain-text stdout, we spawn `thclaws --serve` once per
 * Paperclip process (lazy), then make OpenAI chat completion calls
 * over the local HTTP listener. This unlocks:
 *
 *   - streaming text deltas (SSE)
 *   - tool-call events (`x_thclaws_tool_use`) rendered in transcript
 *   - usage tallies for billing
 *   - shared HTTP client code with the `thclaws_pod` adapter
 *
 * Why the change: thClaws's `-p --output-format stream-json` flag is
 * declared in the CLI but not wired through `run_print_mode`, so the
 * stream-json events we'd want to parse don't actually exist there.
 * `thclaws --serve` already emits them via its OpenAI-compatible SSE.
 */
import type {
  AdapterExecutionContext,
  AdapterExecutionResult,
} from "@paperclipai/adapter-utils";
import { runChat } from "./http-client.js";
import { getLocalThclawsEndpoint } from "./spawn-lifecycle.js";
import { tokensToCostUsd } from "./pricing.js";

function asString(config: Record<string, unknown>, key: string, fallback: string): string {
  const v = config[key];
  return typeof v === "string" && v.trim().length > 0 ? v : fallback;
}

function asOptionalString(config: Record<string, unknown>, key: string): string | null {
  const v = config[key];
  return typeof v === "string" && v.trim().length > 0 ? v : null;
}

function asOptionalNumber(config: Record<string, unknown>, key: string): number | null {
  const v = config[key];
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function asEnvRecord(config: Record<string, unknown>, key: string): Record<string, string> {
  const v = config[key];
  if (!v || typeof v !== "object" || Array.isArray(v)) return {};
  const out: Record<string, string> = {};
  for (const [k, val] of Object.entries(v)) {
    if (typeof val === "string" && val.length > 0) out[k] = val;
  }
  return out;
}

export async function execute(
  ctx: AdapterExecutionContext,
): Promise<AdapterExecutionResult> {
  const config = (ctx.config ?? {}) as Record<string, unknown>;

  const command = asString(config, "command", "thclaws");
  const cwd = asString(config, "cwd", process.cwd());
  const model = asString(config, "model", "claude-sonnet-4-6");
  const systemPrompt = asOptionalString(config, "systemPrompt");
  const temperature = asOptionalNumber(config, "temperature");
  const maxTokens = asOptionalNumber(config, "maxTokens");
  // `mode: "async"` opts into thClaws's x_callback extension — fire-
  // and-forget at the wire level, with a single terminal webhook
  // delivered to the orchestrator's PAPERCLIP_API_URL when the agentic
  // loop finishes. Requires PAPERCLIP_API_URL + PAPERCLIP_API_KEY +
  // PAPERCLIP_RUN_ID env (set by the orchestrator before invoking the
  // adapter). Missing precondition ⇒ log + fall back to sync, never
  // an error — async is opt-in, never load-bearing.
  // See dev-plan/23-thclaws-async-callback.md.
  const mode = asString(config, "mode", "sync");
  const xCallback = mode === "async" ? deriveXCallback(ctx, model) : null;
  // Paperclip's secret-resolution layer injects provider API keys
  // (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.) into config.env per
  // execute. They need to reach the thclaws --serve subprocess so
  // its upstream provider routing finds them.
  const envOverrides = asEnvRecord(config, "env");

  // Paperclip's heartbeat populates context with several markdown
  // fields rather than a single `.prompt` string. Mirror the
  // assembly thcompany's claude-local uses (joinPromptSections):
  // wake reason → session handoff → task body → legacy .prompt.
  const ctxAny = ctx.context as Record<string, unknown>;
  const sections = [
    asString(ctxAny, "wakePrompt", ""),
    asString(ctxAny, "paperclipSessionHandoffMarkdown", ""),
    asString(ctxAny, "paperclipTaskMarkdown", ""),
    asString(ctxAny, "prompt", ""),
  ]
    .map((s) => s.trim())
    .filter((s) => s.length > 0);

  if (sections.length === 0) {
    return {
      exitCode: 1,
      signal: null,
      timedOut: false,
      errorCode: "empty_prompt",
      errorFamily: null,
      errorMessage:
        "thclaws_local execute() got no prompt content — context had no wakePrompt, paperclipSessionHandoffMarkdown, paperclipTaskMarkdown, or .prompt field.",
      model,
    };
  }

  const userPrompt = sections.join("\n\n");

  await ctx.onMeta?.({
    adapterType: "thclaws_local",
    command: `${command} --serve (via local HTTP)`,
    cwd,
    prompt: userPrompt,
    context: { model, transport: "local-http", streaming: true },
  });

  // Lazy spawn the per-process thclaws --serve daemon. First call
  // pays the spawn cost (~1-3s); subsequent calls share the endpoint.
  // Per-call envOverrides only apply on the first spawn — singleton
  // semantics. To rotate keys, restart the parent process.
  let endpoint;
  try {
    endpoint = await getLocalThclawsEndpoint({ command, cwd, env: envOverrides });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return {
      exitCode: 1,
      signal: null,
      timedOut: false,
      errorCode: "spawn_failed",
      errorFamily: null,
      errorMessage: `Could not start thclaws --serve: ${msg}`,
      model,
    };
  }

  const result = await runChat({
    baseUrl: endpoint.baseUrl,
    bearerToken: endpoint.bearerToken,
    model,
    systemPrompt,
    userPrompt,
    temperature,
    maxTokens,
    onLogStdout: (chunk) => ctx.onLog("stdout", chunk),
    onLogStderr: (chunk) => ctx.onLog("stderr", chunk),
    ...(xCallback ? { xCallback } : {}),
  });

  if (result.asyncAccepted) {
    await ctx.onLog(
      "stderr",
      `[async] thClaws accepted run ${result.acceptedRunId ?? "(unknown)"} — awaiting webhook callback at ${xCallback?.url}\n`,
    );
    // `status: "running_async"` is an additive AdapterExecutionResult
    // field introduced in dev-plan/23. The npm-published @paperclipai/
    // adapter-utils we depend on here doesn't carry it yet — the local
    // packages/adapter-utils source has been updated, so the next
    // release will pick it up and this cast becomes unnecessary.
    return {
      exitCode: null,
      signal: null,
      timedOut: false,
      status: "running_async",
      model,
    } as AdapterExecutionResult & { status: "running_async" };
  }

  if (result.ok) {
    // dev-plan/24: compute cost locally from token counts × bundled
    // pricing table. thClaws never emits cost_usd on the wire — this
    // is the consumer's responsibility, and the bundled snapshot lets
    // us compute even when the live thClaws version is older than
    // this adapter.
    const costUsd =
      result.usage !== undefined
        ? tokensToCostUsd(model, {
            prompt_tokens: result.usage.inputTokens,
            completion_tokens: result.usage.outputTokens,
            cached_input_tokens: result.usage.cachedInputTokens,
            cache_creation_tokens: result.usage.cacheCreationInputTokens,
            reasoning_tokens: result.usage.reasoningOutputTokens,
          })
        : null;
    return {
      exitCode: 0,
      signal: null,
      timedOut: false,
      model,
      summary: result.summary,
      usage: result.usage,
      ...(costUsd !== null ? { costUsd } : {}),
    };
  }

  return {
    exitCode: 1,
    signal: null,
    timedOut: false,
    errorCode: result.errorCode,
    errorFamily: result.upstreamError ? "transient_upstream" : null,
    errorMessage: result.errorMessage,
    model,
    summary: result.summary,
    usage: result.usage,
  };
}

/**
 * Build the x_callback envelope from PAPERCLIP_* values the orchestrator
 * passed in via `ctx.config.env`. Falls back to `process.env.*` for
 * standalone / desktop use where the orchestrator doesn't go through
 * the heartbeat env injection path. Returns null (and logs) if any
 * field is missing so the caller transparently falls back to sync.
 */
function deriveXCallback(
  ctx: AdapterExecutionContext,
  model: string,
): { url: string; apiKey: string; runId: string } | null {
  const configEnv = asEnvRecord(ctx.config as Record<string, unknown>, "env");
  const url = (configEnv.PAPERCLIP_API_URL ?? process.env.PAPERCLIP_API_URL ?? "").trim();
  const apiKey = (configEnv.PAPERCLIP_API_KEY ?? process.env.PAPERCLIP_API_KEY ?? "").trim();
  const runId = (configEnv.PAPERCLIP_RUN_ID ?? process.env.PAPERCLIP_RUN_ID ?? "").trim();
  const missing: string[] = [];
  if (!url) missing.push("PAPERCLIP_API_URL");
  if (!apiKey) missing.push("PAPERCLIP_API_KEY");
  if (!runId) missing.push("PAPERCLIP_RUN_ID");
  if (missing.length > 0) {
    void ctx.onLog(
      "stderr",
      `[async] mode=async requested but ${missing.join(", ")} not set — falling back to sync (model=${model})\n`,
    );
    return null;
  }
  // Build the conventional callback URL. Orchestrators that want a
  // different path can set PAPERCLIP_API_URL to the full URL including
  // path; we only append the path if PAPERCLIP_API_URL looks like a
  // bare origin (no path beyond /).
  let callbackUrl = url!;
  try {
    const parsed = new URL(callbackUrl);
    if (parsed.pathname === "" || parsed.pathname === "/") {
      parsed.pathname = `/api/runs/${runId!}/complete`;
      callbackUrl = parsed.toString();
    }
  } catch {
    // If URL parsing fails downstream validation in thClaws will catch
    // it. Don't add fallback handling that masks a real misconfiguration.
  }
  return { url: callbackUrl, apiKey: apiKey!, runId: runId! };
}
