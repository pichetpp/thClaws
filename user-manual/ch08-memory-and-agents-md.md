# Chapter 8 — Memory & project instructions

Two separate systems feed long-lived context into the model's system
prompt at startup:

1. **Project instructions** — static rules about the codebase, written
   once (and checked in): `CLAUDE.md`, `AGENTS.md`, `.claude/rules/*.md`.
2. **Memory** — dynamic notes the agent writes and reads during work:
   `MEMORY.md` + per-topic files under `.thclaws/memory/`.

Both end up in the system prompt, so the agent sees them on every
turn. Kept small, they improve continuity across sessions.

## Project instructions (`CLAUDE.md` / `AGENTS.md`)

Put a file named `CLAUDE.md` or `AGENTS.md` at your project root
describing the conventions you want the agent to follow:

```markdown
# Project conventions

- Language: Rust 2021, `cargo fmt` before every commit.
- Tests live alongside code in `#[cfg(test)]` modules.
- Never touch files under `vendor/`.
- Prefer `anyhow::Result` over `Box<dyn Error>` in application code.
- Commit messages: imperative mood, ≤72 chars in the first line.
```

**Both names are supported.** `AGENTS.md` is the vendor-neutral standard
from Google / OpenAI / Factory / Sourcegraph / Cursor (stewarded by the
Agentic AI Foundation). `CLAUDE.md` is Claude Code's original
convention. If both exist at the same location, both are included —
`CLAUDE.md` first so per-vendor refinements can sit on top of a shared
baseline.

### Where thClaws looks

Loaded in this order (later entries refine / override earlier ones):

1. `~/.claude/CLAUDE.md`, `~/.claude/AGENTS.md`,
   `~/.config/thclaws/AGENTS.md`, `~/.config/thclaws/CLAUDE.md` —
   user-global baseline
2. Walk up from cwd: `CLAUDE.md` + `AGENTS.md` in each ancestor
   directory (root-most first)
3. Project config dirs, in this order:
   `.claude/CLAUDE.md`, `.thclaws/CLAUDE.md`, `.thclaws/AGENTS.md`
4. Rules dirs — every `.md` file alphabetically, first from
   `.claude/rules/` then from `.thclaws/rules/`
5. `CLAUDE.local.md`, `AGENTS.local.md` — local overrides, typically
   gitignored, highest priority

Run `/context` in the REPL to see the combined system prompt.

### What belongs here vs in memory

`CLAUDE.md` / `AGENTS.md` is for **things you'd tell every new hire**:
"use Prisma, not Drizzle"; "API endpoints go in `api/v2/`"; "log in
JSON, never plain text". Static, long-lived.

Memory is for **things the agent learns**: "user prefers concise
answers"; "the Stripe webhook failure last month was a clock-skew
bug, not invalid signing".

## Memory

Memory lives at `.thclaws/memory/`:

```
.thclaws/memory/
├── MEMORY.md              one-line index (what files exist, what they cover)
├── user_preferences.md    what the user likes, disliked approaches, past corrections
├── project_context.md     in-flight work, deadlines, why decisions were made
└── reference_links.md     "bugs are tracked in Linear ENG project", "staging URL is …"
```

### Writing memory

The agent writes memory through three tools, all gated by the normal
permission system:

- **`MemoryWrite`** — create or replace an entry. Auto-stamps the
  frontmatter (`name`, `created`, `updated`) and auto-updates the
  `MEMORY.md` index. Asks for approval before it writes.
- **`MemoryAppend`** — append to an existing entry and bump `updated:`.
- **`MemoryRead`** — fetch the full body of an entry the system prompt
  marked as `body deferred` (elided to keep the prompt under budget).

So you can just say "remember that I prefer TypeScript over plain JS"
and the agent files it through the permission gate — no hand-editing
required. You can still open any `*.md` under
`~/.local/share/thclaws/memory/` (or `./.thclaws/memory/` for a
project-scoped note) and edit it by hand; the agent re-reads these
files every turn. The write tools deliberately bypass the filesystem
sandbox to land inside the resolved memory root (the same carve-out as
`TodoWrite` and `KmsWrite`), with path safety enforced separately.

Each memory file has YAML frontmatter:

```markdown
---
name: project_context
description: Ongoing context about the Q2 refactor
type: project
---

The billing module rewrite is blocked on legal review of the new
pricing tiers. Target unblock date: 2026-09-15. Contact: Priya.
```

Types thClaws recognises: `user`, `feedback`, `project`, `reference`.
The list lives in `MEMORY.md` as one-line pointers; the full file body
is only loaded when the agent explicitly asks for it (via `/memory read
NAME`).

### Memory commands

```
❯ /memory
  user_preferences [user] — what the user likes and dislikes
  project_context [project] — ongoing Q2 refactor notes
  …

❯ /memory read project_context
(prints the full file body)
```

### Memory vs session history

Memory persists **across sessions and across machines** (if you check
it into git). Session history is per-conversation — useful to resume
a specific thread but not a knowledge base.

Rule of thumb: if it's still true in a month, it belongs in memory. If
it's true right now for this task, it belongs in the conversation.

## Size budget

Both `CLAUDE.md`/`AGENTS.md` and memory go into the system prompt, so
they cost tokens every turn. Aim for:

- `CLAUDE.md` / `AGENTS.md`: under 1 KB.
- `MEMORY.md` (index): under 500 bytes.
- Each topic memory file: under 1 KB.

For bigger context, put it in a regular file and let the agent `Read`
it only when relevant.
