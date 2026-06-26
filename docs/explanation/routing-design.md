# Why Routing Works This Way

## The problem

Claude Code sends a single model name on every request. But you rarely want every request to hit the same real model. A background or haiku-class task should hit a cheap model. A plan-mode request should hit a thinking model. A request that uses a `web_search` tool should hit a search-capable model. Everything else should hit a sensible default. And once you have picked a friendly model name, you want to map it onto a prioritized list of real providers with fallback.

So routing has two jobs. First, look at one incoming model name plus the request's intent and decide which final model name applies. Second, hand that name to model resolution, which turns it into an ordered list of real providers. This doc is about the first job. See [../reference/routing.md](../reference/routing.md) for the config that drives it.

## The priority pipeline

`Router::route()` runs a fixed pipeline. The order is a PRIORITY order, not a sequence of steps that all run. The first rule that matches wins and short-circuits the rest:

1. **WebSearch** - a `web_search` tool in the request is a strong signal, so it goes first.
2. **Subagent** - a `cc_is_subagent=true` billing header flag.
3. **Think** - plan mode.
4. **Background** - the cheap path for haiku-class traffic.
5. **Auto-map** - a bulk rewrite of `claude-*` names to your default.
6. **Default** - the fallback for everything else.

Read it top to bottom and stop at the first match. WebSearch beats Think, Think beats Background, and so on.

## Why auto-map runs last

This is the part that is easy to get wrong. The higher-priority rules (WebSearch, Subagent, Think, Background) all inspect the ORIGINAL model name and the request's intent. Background detection, for example, matches on something like `(?i)claude.*haiku`.

Now imagine auto-map ran first. Auto-map rewrites `claude-*` to your default model. So `claude-haiku` would become (say) `gpt-4o` before background detection ever looked at it. Background detection would never see the word "haiku", and the request would misroute to the default instead of the cheap path.

So auto-map has to run AFTER the rules that depend on the original name. It is a catch-all rewrite that only fires when nothing more specific matched, right before the default fallback. The specific rules get first look at the real name; auto-map sweeps up whatever is left.

## Why defined models skip auto-map

There is a second subtlety. Suppose you define a `[[models]]` entry named `claude-sonnet-4` with its own mappings. You did that on purpose. You want `claude-sonnet-4` routed by ITS mappings, not bulk-rewritten to the default just because its name starts with `claude-`.

So a model whose name matches `auto_map_regex` but is ALSO an explicitly defined model bypasses the rewrite. This lets you keep a broad `auto_map_regex = "^claude-"` for generic Claude traffic while pinning specific `claude-*` names to exact providers.

The trade-off: the rule is name-based. The `[[models]].name` must exactly equal the requested name for the bypass to apply. There is no pattern matching on the defined-model side, only exact-name matching.

## The subagent detection change

The subagent rule used to rely on a `<CCM-SUBAGENT-MODEL>` tag embedded in the system prompt. It now detects subagent requests by scanning for `cc_is_subagent=true` in the billing header block of the system prompt. This is more reliable: the billing header is a structured signal, not a free-text tag that could appear in any block.

The old tag behavior — extracting the tag's model name and falling through to later routing stages when `router.subagent` was not configured — has been removed. Now the detection is a simple flag: present and configured → route to `router.subagent`; present but not configured → fall through to Think/Background/Auto-map/Default with the original model name; not present → fall through.

Legacy `<CCM-SUBAGENT-MODEL>` tags are still stripped from the prompt for backward compatibility, but they no longer influence routing.

## Trade-offs

- **Order is hard-coded.** The priority order is fixed in the pipeline, not configurable per deployment. This keeps behavior predictable and easy to reason about, but it means you cannot reorder, say, Think above WebSearch without changing code.
- **Regex on the original name.** Detection relies on the incoming name still being intact, which is exactly why auto-map runs last. If you write detection regexes that depend on a rewritten name, they will not match.
- **Exact-name bypass only.** Pinning a `claude-*` model past auto-map requires defining it with the exact requested name.

## See also

- [../reference/routing.md](../reference/routing.md) - the routing config reference.
