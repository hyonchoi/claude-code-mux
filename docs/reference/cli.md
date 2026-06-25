# CLI Reference

The `ccm` binary controls the proxy from the command line. This page lists every subcommand, its flags, and the global flags and environment variables that apply to all of them.

| Command | Flags | Description |
| --- | --- | --- |
| `start` | `--port`, `-p <u16>` | Starts the proxy, writes a PID file, prints the router config banner. |
| `stop` | none | Reads the PID file and terminates the running server. |
| `restart` | none | Stops, then relaunches `ccm start` detached. |
| `status` | none | Reports whether the server is running or the PID file is stale. |
| `model` | none | Prints the configured router models and enabled providers. |
| `init` | none | Stub that prints guidance. Not implemented yet. |

## start

Starts the proxy. `--port` overrides `server.port` from the config. `ccm` writes a PID file and prints the router config banner, which reads the version from the `VERSION` file.

```bash
ccm start
ccm start --port 13456
ccm start -c /path/to/config.toml
```

## stop

Reads the PID file and terminates the running server. On Unix it sends `SIGTERM`.

```bash
ccm stop
```

## restart

Stops the running server, then relaunches `ccm start` detached. If you started with `--config`, `restart` re-passes it.

```bash
ccm restart
```

## status

Reports running or stale based on the PID file.

```bash
ccm status
```

## model

Prints the configured router models and the enabled providers.

```bash
ccm model
```

## init

Prints setup guidance. It is currently a stub and does not create anything.

```bash
ccm init
```

## Global flags

| Flag | Description |
| --- | --- |
| `--config`, `-c <PATH>` | Config file path. Defaults to `~/.claude-code-mux/config.toml`. |
| `--version`, `-V` | Prints the version and exits. |

```bash
ccm --version
ccm --config /path/to/config.toml start
```

## Environment variables

`RUST_LOG` controls log verbosity.

```bash
RUST_LOG=ccm=info ccm start
RUST_LOG=ccm=debug ccm start
RUST_LOG=ccm=trace ccm start
```

At `trace`, OpenAI request header logging shows custom header keys only, never values, and `Authorization` is shown as `Bearer ***`.

## Related

- [Configuration Reference](../reference/configuration.md)
