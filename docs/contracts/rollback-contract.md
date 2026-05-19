# Rollback Contract: Passthrough / Fallback Toggle

## Toggle Key

Config path: `server.enable_fallback` (boolean, default: `true`)

Full TOML example:
```toml
[server]
enable_fallback = false   # disable fallback/passthrough routing
```

## Scope

Global — applies to all providers and all model mappings simultaneously.
Per-provider granularity is out of scope for this contract.

## Propagation Semantics

**Restart-based. Hard stop. No graceful drain.**

- The toggle takes effect only after a server restart (`ccm` process restart).
- In-flight requests at the time of restart are dropped by the OS; the caller
  receives a connection reset. This is consistent with the existing restart
  behavior for all config changes.
- There is no graceful drain window. Adding one would require TOML + in-memory
  coordination, which is self-contradictory (TOML is durable; graceful drain
  requires in-memory state that survives past the config reload boundary).

## Verification Steps

1. Set `enable_fallback = false` in your TOML config file.
2. Send a POST `/api/restart` or restart the `ccm` process.
3. Send a request that would normally trigger fallback routing.
4. Expected: router returns a 502/503 with no fallback attempted.
5. Confirm in server logs: no `🔄 Trying mapping` lines appear after the restart.

## Out of Scope

- UI toggle in admin.html (TOML config only)
- Per-provider granularity
- Graceful drain / in-flight request completion
