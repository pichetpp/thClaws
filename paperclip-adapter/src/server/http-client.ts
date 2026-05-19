/**
 * OpenAI Chat Completions HTTP client for thClaws's --serve API.
 *
 * Transport-agnostic: takes a baseUrl + bearer token, doesn't care if
 * the listener is a local subprocess (thclaws_local) or a remote pod
 * (thclaws_pod). Shared by both adapters.
 *
 * Wire protocol: POST {baseUrl}/chat/completions with `stream: true`
 * yields Server-Sent Events. Content deltas land on stdout; thClaws's
 * `x_thclaws_tool_use` extension events land on stderr as JSON lines
 * the UI parser picks up. See thclaws-technical-manual/openai-api.md
 * for the full reference.
 */

export interface RunChatRequest {
  baseUrl: string;
  bearerToken: string;
  model: string;
  systemPrompt: string | null;
  userPrompt: string;
  temperature: number | null;
  maxTokens: number | null;
  /** Forwarded to ctx.onLog by the caller. */
  onLogStdout: (chunk: string) => Promise<void> | void;
  onLogStderr: (chunk: string) => Promise<void> | void;
  /**
   * Opt the request into thClaws's `x_callback` extension — fire-and-
   * forget with a single terminal webhook delivery. When set, the body
   * carries an `x_callback` object instead of `stream: true`, the
   * response is parsed as 202 Accepted, and the SSE consumption path
   * is bypassed entirely. The caller is responsible for the receiver
   * side. See `dev-plan/23-thclaws-async-callback.md`.
   */
  xCallback?: {
    url: string;
    apiKey: string;
    runId: string;
    idempotencyKey?: string;
  };
}

export interface RunChatResult {
  ok: boolean;
  summary: string | null;
  finishReason: string | null;
  usage?: {
    inputTokens: number;
    outputTokens: number;
    /** dev-plan/24: extra token-type counts from thClaws's usage
     *  block. Used by paperclip-adapter's local cost compute. */
    cachedInputTokens?: number;
    cacheCreationInputTokens?: number;
    reasoningOutputTokens?: number;
  };
  errorMessage?: string;
  errorCode?: string;
  /** `true` if the error was from upstream (network, 5xx, mid-stream) — caller maps to errorFamily=transient_upstream. */
  upstreamError?: boolean;
  /**
   * Set by the async path when thClaws ACK'd with 202 Accepted. The
   * actual run continues server-side and the result lands at the
   * receiver via webhook. Sync callers ignore; the paperclip execute
   * layer maps this to `status: "running_async"` on AdapterExecutionResult.
   */
  asyncAccepted?: boolean;
  /** Run id thClaws echoed back in the 202 body — should match xCallback.runId. */
  acceptedRunId?: string;
}

interface OpenAiMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

interface ChatCompletionChunk {
  id?: string;
  model?: string;
  choices?: Array<{
    index?: number;
    delta?: { role?: string; content?: string };
    finish_reason?: string | null;
  }>;
  usage?: {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
    // dev-plan/24: thClaws extends the usage block with these
    // optional per-token-type counts so consumers can compute cost.
    cached_input_tokens?: number;
    cache_creation_input_tokens?: number;
    reasoning_output_tokens?: number;
  };
  x_thclaws_tool_use?: {
    id: string;
    name: string;
    status: "started" | "completed" | "error" | "denied";
    input?: unknown;
    output?: { preview?: string; truncated?: boolean; total_chars?: number };
  };
}

export async function runChat(req: RunChatRequest): Promise<RunChatResult> {
  const messages: OpenAiMessage[] = [];
  if (req.systemPrompt && req.systemPrompt.trim().length > 0) {
    messages.push({ role: "system", content: req.systemPrompt });
  }
  messages.push({ role: "user", content: req.userPrompt });

  const body: Record<string, unknown> = {
    model: req.model,
    messages,
  };
  if (req.xCallback) {
    body.x_callback = {
      url: req.xCallback.url,
      api_key: req.xCallback.apiKey,
      run_id: req.xCallback.runId,
      ...(req.xCallback.idempotencyKey
        ? { idempotency_key: req.xCallback.idempotencyKey }
        : {}),
    };
  } else {
    body.stream = true;
  }
  if (req.temperature !== null) body.temperature = req.temperature;
  if (req.maxTokens !== null) body.max_tokens = req.maxTokens;

  let res: Response;
  try {
    res = await fetch(`${req.baseUrl}/chat/completions`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: req.xCallback ? "application/json" : "text/event-stream",
        authorization: `Bearer ${req.bearerToken}`,
      },
      body: JSON.stringify(body),
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return {
      ok: false,
      summary: null,
      finishReason: null,
      errorCode: "transport_unreachable",
      errorMessage: `Could not reach thClaws --serve endpoint at ${req.baseUrl}: ${msg}`,
      upstreamError: true,
    };
  }

  // Async path: thClaws ACKs with 202 + small JSON ack body and runs
  // the agentic loop in the background. The final result lands at
  // xCallback.url out-of-band — we have nothing more to do here.
  if (req.xCallback) {
    if (res.status === 202) {
      let runId = req.xCallback.runId;
      try {
        const ack = (await res.json()) as { run_id?: string };
        if (typeof ack.run_id === "string" && ack.run_id.length > 0) {
          runId = ack.run_id;
        }
      } catch {
        // ACK body is optional/best-effort — fall back to the run id we sent.
      }
      return {
        ok: true,
        summary: null,
        finishReason: "accepted_async",
        asyncAccepted: true,
        acceptedRunId: runId,
      };
    }
    if (!res.ok) {
      const text = await res.text().catch(() => "(no body)");
      return {
        ok: false,
        summary: null,
        finishReason: null,
        errorCode: res.status === 401 ? "invalid_api_key" : `http_${res.status}`,
        errorMessage: `thClaws rejected async request: ${res.status} ${text.slice(0, 500)}`,
        upstreamError: res.status >= 500,
      };
    }
    // 2xx but not 202 — server didn't honour the async extension. Treat
    // as a protocol violation; the caller should fall back to sync.
    return {
      ok: false,
      summary: null,
      finishReason: null,
      errorCode: "async_not_supported",
      errorMessage: `thClaws returned ${res.status} but async mode requested — server may not support x_callback`,
      upstreamError: false,
    };
  }

  if (!res.ok || !res.body) {
    const text = await res.text().catch(() => "(no body)");
    return {
      ok: false,
      summary: null,
      finishReason: null,
      errorCode: res.status === 401 ? "invalid_api_key" : `http_${res.status}`,
      errorMessage: `thClaws --serve returned ${res.status}: ${text.slice(0, 500)}`,
      upstreamError: res.status >= 500,
    };
  }

  const stdoutChunks: string[] = [];
  let finishReason: string | null = null;
  let midStreamError: string | null = null;
  let usage: RunChatResult["usage"];

  try {
    for await (const chunk of parseSseStream(res.body)) {
      if (chunk.x_thclaws_tool_use) {
        await req.onLogStderr(`[tool] ${JSON.stringify(chunk.x_thclaws_tool_use)}\n`);
        continue;
      }
      const choice = chunk.choices?.[0];
      const content = choice?.delta?.content;
      if (typeof content === "string" && content.length > 0) {
        stdoutChunks.push(content);
        await req.onLogStdout(content);
        if (content.startsWith("[thclaws error]")) {
          midStreamError = content.trim();
        }
      }
      if (choice?.finish_reason) {
        finishReason = choice.finish_reason;
      }
      if (chunk.usage) {
        usage = {
          inputTokens: chunk.usage.prompt_tokens ?? 0,
          outputTokens: chunk.usage.completion_tokens ?? 0,
          ...(chunk.usage.cached_input_tokens !== undefined
            ? { cachedInputTokens: chunk.usage.cached_input_tokens }
            : {}),
          ...(chunk.usage.cache_creation_input_tokens !== undefined
            ? { cacheCreationInputTokens: chunk.usage.cache_creation_input_tokens }
            : {}),
          ...(chunk.usage.reasoning_output_tokens !== undefined
            ? { reasoningOutputTokens: chunk.usage.reasoning_output_tokens }
            : {}),
        };
      }
    }
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return {
      ok: false,
      summary: stdoutChunks.join("") || null,
      finishReason,
      usage,
      errorCode: "sse_parse_failed",
      errorMessage: `SSE stream broke after ${stdoutChunks.length} chunks: ${msg}`,
      upstreamError: true,
    };
  }

  const summary = stdoutChunks.join("").trim() || null;

  if (midStreamError || finishReason === "error") {
    return {
      ok: false,
      summary,
      finishReason,
      usage,
      errorCode: "upstream_error",
      errorMessage: midStreamError ?? `thClaws finished with finish_reason=${finishReason}`,
      upstreamError: true,
    };
  }

  return {
    ok: true,
    summary,
    finishReason,
    usage,
  };
}

/**
 * Minimal SSE parser. Yields each `data:` JSON event as a parsed object.
 * Ignores `:keepalive` comments and the terminal `[DONE]` sentinel.
 */
async function* parseSseStream(body: ReadableStream<Uint8Array>): AsyncIterable<ChatCompletionChunk> {
  const reader = body.getReader();
  const decoder = new TextDecoder("utf-8");
  let buf = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let idx: number;
    while ((idx = buf.indexOf("\n\n")) !== -1) {
      const rawEvent = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      const dataLines = rawEvent
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).trimStart());
      if (dataLines.length === 0) continue;
      const dataStr = dataLines.join("\n");
      if (dataStr === "[DONE]") return;
      try {
        yield JSON.parse(dataStr) as ChatCompletionChunk;
      } catch {
        /* drop unparseable */
      }
    }
  }
  if (buf.trim().length > 0) {
    const dataLines = buf
      .split("\n")
      .filter((l) => l.startsWith("data:"))
      .map((l) => l.slice(5).trimStart());
    const dataStr = dataLines.join("\n");
    if (dataStr && dataStr !== "[DONE]") {
      try {
        yield JSON.parse(dataStr) as ChatCompletionChunk;
      } catch {
        /* drop */
      }
    }
  }
}
