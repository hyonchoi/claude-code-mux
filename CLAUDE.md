## Commands

```bash
cargo build            # compile
cargo run              # run dev server (default port 3000)
cargo run --release    # production build
cargo test             # run all tests
cargo test <name>      # run a single test (add -- --nocapture to see output)
cargo clippy           # lint
cargo fmt              # format
RUST_LOG=ccm=info cargo run  # debug mode — logs Copilot session IDs, OAuth refresh, retries
```

Binary name: `ccm`. Config file: TOML, set via `--config <path>` (see `config/example.toml`).

## Codebase Structure

- `src/main.rs` — CLI entry point (`--port`, `--config` flags)
- `src/cli/` — CLI argument parsing (subcommands and flags)
- `src/server/` — HTTP server, admin UI (`admin.html`), OAuth handlers
- `src/router/` — intelligent routing logic (task-type detection, provider fallback)
- `src/providers/` — per-provider API adapters
- `src/models/` — config types and model mapping
- `src/auth/` — API key and OAuth credential management
- `config/` — example TOML configs (`example.toml`, `copilot.example.toml`, `claude-max-oauth.example.toml`, etc.); `config/templates/` has provider-specific examples

---

# RapidSpec Instructions

AI assistant instructions for this project using RapidSpec workflow.

Always reference `@/rapidspec/AGENTS.md` when:
- Planning features or changes
- Creating proposals or specs
- Making architectural decisions
- Need clarification on workflow

See `@/rapidspec/AGENTS.md` for:
- RapidSpec workflow and commands
- Spec-driven development process
- AI agents and their usage
- Project conventions

Keep this file so `rapid update` can refresh instructions.

## Architecture Guidelines

### Admin UI State Management

The admin UI (`src/server/admin.html`) uses two complementary state management patterns:

#### 1. URL-based State Management
See `@docs/url-state-management.md` for detailed documentation.

- Navigation state (tabs, views) is stored in URL parameters
- Enables shareable URLs, browser history, and bookmarking
- Example: `?tab=providers&view=add`

#### 2. LocalStorage-based State Management
See `@docs/localstorage-state-management.md` for detailed documentation.

**Critical Architecture Decision**: The server loads TOML config on startup and **does not reload until restart**. Therefore:

- ✅ **Correct**: Use localStorage as client-side cache
  - Page load: Fetch from server → save to localStorage
  - All operations (add/delete/edit): Update localStorage only
  - "저장" button: Sync localStorage → server
  - "저장 & 재시작" button: Sync → restart server

- ❌ **Wrong**: Fetch from server after each operation
  - Server returns stale data until restart
  - Causes inconsistent UI state

**Key Functions**:
- `loadConfig()` - Fetch from server (only on page load)
- `saveToLocalStorage(config)` - Save to localStorage
- `syncToServer()` - Sync localStorage to server (only called by save buttons)
- All CRUD operations update localStorage only, then call `saveToLocalStorage()`

**When modifying admin UI**:
1. Never add server fetches in CRUD operations
2. All operations must update `appState.config` → `saveToLocalStorage()`
3. Only save buttons call `syncToServer()`
4. Always notify user: "(저장 버튼을 눌러 적용하세요)"

**No JS test framework** — verification is browser DevTools console only. Call the changed function directly to test.

**UIKit 3** is loaded via CDN (`uikit@3/dist/js/uikit.min.js`). Use `UIkit.modal.confirm()` / `UIkit.modal.prompt()` for dialogs and `UIkit.notification()` for toasts. Never use native `alert()`, `confirm()`, or `prompt()`.

<!-- code-review-graph MCP tools -->
## MCP Tools: code-review-graph

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
code-review-graph MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes` or `query_graph` instead of Grep
- **Understanding impact**: `get_impact_radius` instead of manually tracing imports
- **Code review**: `detect_changes` + `get_review_context` instead of reading entire files
- **Finding relationships**: `query_graph` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview` + `list_communities`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### Key Tools

| Tool | Use when |
| ------ | ---------- |
| `detect_changes` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context` | Need source snippets for review — token-efficient |
| `get_impact_radius` | Understanding blast radius of a change |
| `get_affected_flows` | Finding which execution paths are impacted |
| `query_graph` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes` | Finding functions/classes by name or keyword |
| `get_architecture_overview` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes` for code review.
3. Use `get_affected_flows` to understand impact.
4. Use `query_graph` pattern="tests_for" to check coverage.
