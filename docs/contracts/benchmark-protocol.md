# Benchmark Measurement Protocol for p95/p99 Gates

## Purpose

This protocol defines how latency benchmarks are run to produce valid p95/p99 measurements.
Benchmarks not following this protocol must not be used to gate releases.

## Warmup

- **Warmup iterations:** 100 requests before recording any samples.
- Warmup requests are discarded entirely; they exist to fill JIT caches and connection pools.

## Sample Size

- **Minimum sample size:** 1000 requests after warmup.
- Samples must be collected within a single continuous run (no pausing between samples).

## Environment Requirements

- Run on a machine with no other significant CPU load (< 10% baseline CPU).
- Network: localhost loopback only (no external provider calls during benchmark — use a
  stub/mock provider that returns a fixed response in < 1ms).
- Do not run benchmarks in shared CI environments. Use a dedicated benchmark machine or
  a dedicated CI job that pins CPU affinity.

## Confidence Interval

Report p95 and p99 with a 95% confidence interval computed via bootstrap resampling
(1000 bootstrap samples). If the confidence interval width > 20% of the point estimate,
the sample size is insufficient — double the sample count and re-run.

## Bimodal Distribution Handling

After collecting samples, check for bimodal distribution:
- Plot a histogram with 50 bins.
- If two distinct peaks are visible (separated by a trough where frequency < 25% of the
  lower peak), the distribution is bimodal.
- **If bimodal: report both modes separately** (mean and p99 of each cluster) and
  flag the benchmark result as **INCONCLUSIVE**.
- An INCONCLUSIVE benchmark must NOT be used to gate a release without human review.

## Reporting Format

```
Benchmark: <name>
Date: YYYY-MM-DD
Samples: N (after M warmup)
p50: Xms [CI: ±Yms]
p95: Xms [CI: ±Yms]
p99: Xms [CI: ±Yms]
Bimodal: YES/NO
Result: PASS / FAIL / INCONCLUSIVE
Gate threshold: p99 < Zms
```
