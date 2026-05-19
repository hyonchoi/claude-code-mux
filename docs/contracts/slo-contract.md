# Canonical Observability SLO Contract

## Service Level Objectives

### Latency SLO

**p99 end-to-end request latency < 5000ms** measured at the relay (from request receipt
to first byte of response), excluding provider streaming duration.

Measurement point: relay handler entry → first response byte out.
Excludes: streaming body delivery time after first byte.

### Error Rate SLO

**Error rate < 1%** over any rolling 5-minute window.

"Error" is defined as a response where all fallback providers failed and the relay
returned 5xx to the caller. Provider-level 4xx errors that triggered fallback do not
count as relay errors if another provider succeeded.

## Alert Thresholds

| Signal | Threshold | Severity |
|--------|-----------|----------|
| p99 latency | > 3000ms for 5 min | warning |
| p99 latency | > 5000ms for 2 min | critical |
| Error rate | > 0.5% for 5 min | warning |
| Error rate | > 1% for 2 min | critical |

## Alert Owner

**Primary:** On-call engineer (rotated weekly).
**Escalation:** Project maintainer if unacknowledged within 15 minutes.
