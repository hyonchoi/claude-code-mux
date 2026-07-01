# Request Structure Diff: opus-4.8 vs sonnet-4.6 vs sonnet-5

Comparison of three captured Claude Code request snapshots for the same `hello?`
prompt. Source files: `opus-4-8.json`, `sonnet-4-6.json`, `sonnet-5.json`.

## Shared envelope (identical across all three)

All requests use the same top-level keys and share these values:

- Top-level keys: `model`, `max_tokens`, `system`, `messages`, `tools`,
  `thinking`, `context_management`, `output_config`, `metadata`
- `thinking`: `{"type": "adaptive"}`
- `output_config`: `{"effort": "medium"}`
- `context_management`: `{"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]}`
- `tools`: 150 tools in every request
- `system`: 3 blocks — `[0]` billing header (no cache), `[1]` + `[2]` cached
  (`ttl: 1h`, ephemeral)

Differences fall into three buckets below.

## 1. Real axis: Opus-4.8 gets a leaner payload than the Sonnet family

| metric              | opus-4.8    | sonnet-4.6 | sonnet-5   |
| ------------------- | ----------- | ---------- | ---------- |
| system prompt chars | **7,608**   | 28,341     | 28,341     |
| tools payload chars | **183,462** | 201,311    | 201,311    |
| tools hash (md5)    | `32d73a7f`  | `cef4e13d` | `cef4e13d` |

The sonnets receive a system prompt ~3.7× larger. The extra content is
safety/confirmation guidance that Opus's prompt omits, e.g.:

- "Examples of the kind of risky actions that warrant user confirmation"
  (destructive ops, hard-to-reverse ops, actions visible to others, uploading
  content to third-party tools)
- "When you encounter an obstacle, do not use destructive actions as a shortcut"
  (root-cause over `--no-verify`, investigate unfamiliar state before deleting)

## 2. Eight tools have model-specific descriptions (terse for Opus, verbose for Sonnet)

Same 150 tool **names** everywhere; the other 142 tools are byte-identical across
all three. These 8 differ by description length:

| tool            | opus  | sonnet | delta   |
| --------------- | ----- | ------ | ------- |
| Bash            | 1,304 | 10,643 | +9,339  |
| Agent           | 1,227 | 5,454  | +4,227  |
| WebFetch        | 374   | 1,479  | +1,105  |
| WebSearch       | 307   | 1,317  | +1,010  |
| Read            | 790   | 1,782  | +992    |
| Edit            | 360   | 1,094  | +734    |
| Write           | 240   | 618    | +378    |
| AskUserQuestion | 1,786 | 1,531  | -255    |

Sonnet's descriptions include worked `<example>` blocks and "When not to use"
sections; Opus's are compressed to essentials. `AskUserQuestion` is the only
tool whose description is *longer* for Opus.

## 3. max_tokens and message-history shape

- **max_tokens**: opus **64,000**, sonnet-4.6 **32,000**, sonnet-5 **64,000**.
- **messages**: differ mostly by capture point, not model design
  (opus 2 msgs, sonnet-4.6 3, sonnet-5 6).
- **Structural quirk**: opus and sonnet-5 emit an inline `role: "system"` message
  in the `messages` array (SessionStart hook context), whereas sonnet-4.6 folds
  that hook content into `text` blocks inside the initial `user` message.

## Bottom line

- **sonnet-4.6 and sonnet-5 are structurally near-identical** — same system
  prompt, same tools (identical hash). Their only meaningful request difference
  is `max_tokens` (32k vs 64k); the rest is conversation-history depth.
- **Opus-4.8 is the outlier**: Claude Code ships it a deliberately leaner request
  — shorter system prompt and stripped-down descriptions on the 8
  highest-traffic tools — on the assumption a stronger model needs less
  hand-holding. This also cuts ~38k chars of prompt overhead per call
  (~20.7k system + ~17.8k tools).
