# Getting Started

Welcome! By the end of this tutorial you will have `ccm` running locally, and you will route your first request through it to a real model. You will see the reply come back in Anthropic Messages format. You only need a few minutes and one API key.

`ccm` (claude-code-mux) is a small HTTP proxy. It speaks the Anthropic Messages API on the front, and routes each request to the provider you choose on the back.

## What you'll need

- A Rust toolchain (to build from source), OR a pre-built binary. See the [README install section](../../README.md#installation) for pre-built downloads.
- An API key for one provider. This tutorial uses OpenAI, but any OpenAI-compatible provider works.
- `curl` (already installed on most systems).

## Step 1: Get ccm and start it

If you have the Rust toolchain, build and run from the repo root:

```bash
cargo run -- start
```

(Or build a release binary with `cargo build --release`, then run `target/release/ccm start`.)

The first time you run it, `ccm` creates a default config at `~/.claude-code-mux/config.toml` for you. Then it prints a startup banner with the version and the router configuration:

```
claude-code-mux v0.x.x
Listening on http://127.0.0.1:13456
Router: ...
```

Look at the address in the banner. The generated config uses port `13456`, so this tutorial assumes `http://127.0.0.1:13456`. If your banner prints a different port, use that one everywhere below.

Leave this terminal running. Open a second terminal for the next steps.

## Step 2: Confirm it's alive

Ask the server for its health status:

```bash
curl http://localhost:13456/health
```

You should see:

```json
{"status":"ok","service":"claude-code-mux"}
```

That is your first visible result. The server is up.

## Step 3: Open the Admin UI

Open this URL in your browser:

```
http://localhost:13456/
```

You will see the `ccm` admin interface. This is the easiest way to add a provider, add a model mapping, and set the router default. Take a look around. You now have a running router with a UI in three steps.

## Step 4: Configure one provider, one model, and the router default

Now you will tell `ccm` where to send requests. You can do this in the Admin UI or by editing the config file. Both write the same thing.

First, export your API key in the terminal where `ccm` runs (or before you start it), because the config reads it from an environment variable:

```bash
export OPENAI_API_KEY=sk-...
```

### Option A: Use the Admin UI

1. Add a provider: name it `openai`, set provider type to `openai`, and set the api key to `$OPENAI_API_KEY`.
2. Add a model named `my-model`, and give it a mapping to provider `openai` with actual model `gpt-4o-mini`.
3. Set the router default to `my-model`.
4. Click **Save & Restart**.

The Admin UI caches your changes locally. The server reads its config only at startup, so you must click **Save** (and **Save & Restart** to apply right away). After the restart, the new banner shows your router default.

### Option B: Edit the config file

Open `~/.claude-code-mux/config.toml` and make it look like this:

```toml
[server]
port = 13456

[router]
default = "my-model"

[[providers]]
name = "openai"
provider_type = "openai"
api_key = "$OPENAI_API_KEY"

[[models]]
name = "my-model"

[[models.mappings]]
priority = 1
provider = "openai"
actual_model = "gpt-4o-mini"
```

Here is what each piece does:

- `[[providers]]` defines a backend. `api_key = "$OPENAI_API_KEY"` reads the `OPENAI_API_KEY` environment variable at startup.
- `[[models]]` defines a model name you will call. `[[models.mappings]]` points that name at a real provider model (`gpt-4o-mini` on `openai`).
- `router.default` is the model `ccm` uses when a request does not match anything else.

After editing, stop `ccm` (Ctrl+C) and start it again so it reads the new config:

```bash
cargo run -- start
```

## Step 5: Route your first request

Send a request to the Anthropic Messages endpoint:

```bash
curl http://localhost:13456/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"my-model","max_tokens":100,"messages":[{"role":"user","content":"Say hello in one sentence."}]}'
```

Because `router.default` is `my-model`, and `my-model` maps to `openai/gpt-4o-mini`, `ccm` routes this to OpenAI. The reply comes back in Anthropic Messages format, shaped like this:

```json
{
  "id": "msg_...",
  "type": "message",
  "role": "assistant",
  "content": [
    { "type": "text", "text": "Hello, it is nice to meet you!" }
  ],
  "model": "my-model",
  "stop_reason": "end_turn"
}
```

The `content` array holds the model's text. You just routed a real request through `ccm`.

## Step 6 (optional): Point Claude Code at it

To send Claude Code's requests through `ccm`, set the Anthropic base URL before launching Claude Code:

```bash
export ANTHROPIC_BASE_URL=http://localhost:13456
```

Now Claude Code talks to `ccm`, and `ccm` routes to your chosen provider. For the full Claude Code setup, see the [README](../../README.md).

## What you built

You now have:

- A running `ccm` router on `http://localhost:13456`.
- One provider (`openai`) wired up with your API key.
- One model (`my-model`) mapped to `gpt-4o-mini`.
- A routed request that came back in Anthropic Messages format.

That is the whole loop: a request in, a routing decision, a provider call, an Anthropic-format reply out.

## Next steps

- [Configuration reference](../reference/configuration.md) for every config option.
- [Routing reference](../reference/routing.md) to add more providers, priorities, and fallbacks.
- [CLI reference](../reference/cli.md) for all `ccm` commands and flags.
- [Architecture explanation](../explanation/architecture.md) to understand how `ccm` works inside.
