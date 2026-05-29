# `toonfmt` — MCP passthrough wrapper that converts tool results to TOON

## Goal

A transparent MCP proxy that wraps an existing server and converts JSON tool-result payloads to [TOON](https://toonformat.dev) to reduce the tokens the model ingests per tool call. Declared in `.mcp.json` by prefixing the upstream command, e.g.:

```jsonc
{
  "mcpServers": {
    "sql": {
      "command": "toonfmt",
      "args": ["--", "uvx", "some-sql-mcp", "--db", "..."]
    }
  }
}
```

It spawns the upstream server, pumps JSON-RPC both directions, and transforms only the results of `tools/call`. Everything else passes through untouched.

## Why this shape (decisions already made — don’t relitigate)

- **`content` is the write target, not `structuredContent`.** Coding-agent–style clients (Claude Code, LangChain MCP adapter by default) route the `content` text blocks into the model’s context; `structuredContent` is the programmatic/code-mode channel and is often *not* forwarded to the model. So to actually save model tokens, TOON must land in a `content` text block.
  - ⚠️ Not confirmed from a primary source that Claude Code reads `content` and ignoress `structuredContent`. High-prior assumption. Verify with MCP Inspector before trusting in production (log the on-wire `CallToolResult`, check what the model references next turn).
- **Parse every text block independently; don’t reconcile against `structuredContent`.** `structuredContent` doesn’t map to specific `content[]` blocks, so having it doesn’t save the per-block parse. You have to iterate `content[]` and attempt-parse text blocks anyway. This makes the design simpler than a “read from structuredContent, write to content” split.
- **Don’t TOON-convert `structuredContent` itself.** It has a JSON-object contract on the wire; TOON-as-string would violate it. It arrives already parsed/valid via the JSON-RPC envelope (if it weren’t valid JSON the whole message wouldn’t have parsed). Leave intact.
- **Client variance exists.** Some clients (e.g. Google ADK) forward the *entire* result envelope to the model, including `structuredContent`. For those, TOON in `content` + raw JSON in `structuredContent` doubles tokens. Hence the optional `structuredContent` handling flag below. Default target is content-routing clients.

## Core loop

Pump JSON-RPC over the upstream’s transport (stdio is the common case). Match `tools/call` requests to responses by `id`. Pass through `initialize`, `tools/list`, `prompts/*`, `resources/*`, notifications, etc. unchanged.

For a `tools/call` **result**:

```
if result.isError:           # error payloads are often prose
    pass through unchanged
    return

for block in result.content:               # heterogeneous array
    if block.type != "text":               # image / resource / resource_link
        leave unchanged
        continue
    try:
        obj = strict_json_parse(block.text)
    except:
        try:
            obj = json5_parse(block.text)   # trailing commas, single quotes, etc.
        except:
            leave block unchanged           # genuinely not convertible (prose, etc.)
            continue
    block.text = toon_encode(obj)

# structuredContent handling (see flag):
#   default: leave intact
#   --strip-structured-content: remove it (suppress JSON copy for envelope-forwarding clients)
#   optional: minify it (small win; only if a consumer forwards it to the model)
```

**Fallback is per-block, not per-result** — one un-convertible block must not tank a multi-block result.

**Edge case:** if `structuredContent` is present but `content[]` has no text block (legal, some servers do this), and you want the model to see TOON, you must *add* a text block rather than replace one. Decide whether that’s in scope; default is “only convert existing text blocks.”

## JSON5 strictness boundary

Strictness gradient: `strict json` → **JSON5** → `json-repair`-style salvage. Stop at JSON5.

JSON5 covers the LLM-output flavor (trailing commas, single quotes, unquoted keys, comments). Going further (json-repair) starts *guessing* at unbalanced braces / unterminated strings, which can “succeed” while producing a subtly wrong object — you then emit confident TOON over corrupted data. For unterminated/missing-brace cases, bailing (leave block unchanged) is safer than guessing.

If a non-strict path is taken, **log it** (which upstream server, which tool) so misbehaving servers are auditable rather than silently papered over. If TOON output ever looks subtly wrong, “did JSON5 mis-parse a malformation into a valid-but-wrong object?” is the first thing to check.

## Config surface

- `--skip-tool <glob>` — tools whose results should never be converted (free-form text tools).
- `--structured-content {keep|strip|minify}` — default `keep`.
- `--strictness {strict|json5}` — default `json5`.
- (optional) inject a short TOON syntax cheatsheet into the upstream’s `serverInfo.instructions` during `initialize` so consuming clients that pipe instructions into context don’t each need to add it to their system prompt. ~20 LOC, biggest UX win at the wrapper boundary. (Skipped if the consuming model already handles TOON reliably — which it does in this user’s setup.)

## Language / distribution

- **Single binary (Rust via `toon-rust`, or Go)** — cleanest `.mcp.json` story: `"command": "toonfmt"`, no runtime to install. Preferred.
- **Python + `uv tool install`** — fine, adds a runtime dep. `toon-python` is the stable lib.
- **Node + `npx`** — most idiomatic for the MCP ecosystem, but pays Node startup cost per invocation.

## Validation before trusting it

1. **Confirm the routing assumption**: MCP Inspector against the target client (Claude Code) to verify the model reads `content` and not `structuredContent`. This is the load-bearing assumption for the whole design.
1. TOON comprehension on the actual model is **already confirmed** in this user’s setup (TOON already in use for a SQL MCP, model performs better than with JSON) — no need to re-validate.

## Scope notes

- Tools-only by default. Don’t convert `resources/read` — resources carry markdown/prose that conversion would corrupt.
- This is a design spec, not an implementation. Build the JSON-RPC pump + transform incrementally; don’t one-shot.
