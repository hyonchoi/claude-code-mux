## RTK — Token-Optimized CLI

**rtk** is a CLI proxy that filters and compresses command outputs, saving 60-90% tokens.

### Rule

Always prefix shell commands with `rtk`:

```bash
# Instead of:              Use:
git status                 rtk git status
git log -10                rtk git log -10
cargo test                 rtk cargo test
docker ps                  rtk docker ps
kubectl get pods           rtk kubectl pods
```

### Meta commands (use directly)

```bash
rtk gain              # Token savings dashboard
rtk gain --history    # Per-command savings history
rtk discover          # Find missed rtk opportunities
rtk proxy <cmd>       # Run raw (no filtering) but track usage
```

## gstack + superpowers workflow

gstack and superpowers produce docs in different formats. To bridge them,
use `superpowers:brainstorming` to translate the gstack design doc into a
superpowers spec, then run the rest of superpowers, then gate with gstack
review.

### Canonical sequence

1. **`/office-hours`** (gstack) — produces the design doc at
   `~/.gstack/projects/<slug>/*-design-*.md`.

2. **`superpowers:brainstorming`** — input is the gstack design doc from
   step 1. Restate it in superpowers spec shape (goal / architecture /
   constraints / test coverage / out-of-scope). Do not re-open product
   decisions. Output lands at `docs/superpowers/specs/`.

3. **`superpowers:writing-plans`** — translate the spec into an
   implementation plan with concrete tasks.

4. **`superpowers:subagent-driven-development`** — dispatches fresh subagent
   per task with two-stage review (spec compliance, then code quality)
   with review checkpoints.

5. **gstack review gate** — run `/review` before merging to verify the
   implementation against the plan.
