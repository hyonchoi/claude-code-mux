# Checkpoint Escalation SLA

## What Is a Checkpoint

A checkpoint is a dependency gate that must pass before a release can proceed.

Examples of checkpoints:
- OAuth providers for all configured provider types refresh correctly after server start
- All fallback providers in the active chain respond within SLO
- Admin UI loads without JS errors

## Escalation Chain

| Level | Owner | Response SLA | Trigger |
|-------|-------|-------------|---------|
| L1 | On-call engineer | 15 minutes | Checkpoint fails in staging or prod |
| L2 | Project maintainer | 30 minutes | L1 unresponsive or cannot resolve |
| L3 | Repo owner | 60 minutes | L2 cannot resolve; release decision needed |

## Go/No-Go Decision Authority

**L3 (Repo owner)** has final go/no-go authority for releases when a checkpoint
has not cleared.

L1 and L2 may unblock minor releases (hotfixes) without L3 approval if the
failing checkpoint is explicitly scoped as non-blocking for the release.

## Checkpoint Verification Steps

Before each release:
1. Start a clean server with the release binary and the production TOML config.
2. Confirm all configured OAuth providers exchange tokens successfully.
3. Send one test request per configured model mapping; verify 200 response.
4. Check server logs for any ERROR lines within the first 60 seconds.
5. If all pass → checkpoint cleared. If any fail → L1 escalation triggered.
