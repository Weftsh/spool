# Object-storage latency model

Real S3 (Standard and Express One Zone) is not reachable from the current dev
box, so experiments 003+ run against local MinIO with **latency injected in
the Stratum storage client**, not in the network stack (deterministic,
per-backend, no root tricks). The client is a trait with three impls:
`local-minio` (no injection, for correctness tests), `modeled-standard`,
`modeled-express`. Every results file records which one produced it; modeled
numbers are never reported as real-S3 numbers, and the final table must be
re-validated on real S3 before any verdict is called definitive.

## Parameters (v1 — to be re-calibrated on real S3)

Injected per request, on top of MinIO's own (sub-ms local) cost:

| parameter | S3 Standard | S3 Express One Zone |
|---|---|---|
| GET time-to-first-byte, p50 | 35 ms | 4 ms |
| GET TTFB long-tail (applied to 1% of requests) | 150 ms | 15 ms |
| per-connection sustained throughput cap | 90 MB/s | 200 MB/s |
| PUT TTFB p50 (harness/ingest only) | 45 ms | 5 ms |
| request concurrency | unlimited for our purposes (parallel range GETs are the design's main lever) | same |

Basis: AWS's published positioning (Express = "single-digit millisecond"
first-byte, "up to 10x faster than Standard"; Standard first-byte typically
quoted 30–60 ms in AWS re:Invent material and independent benchmarks;
per-stream throughput commonly measured ~85–100 MB/s on Standard, with
parallelism, not per-stream speed, being how high aggregate throughput is
reached). These are engineering folklore-grade numbers, good enough to rank
layouts and to test the §5 *ratios*; they are not good enough to certify the
absolute 15 ms / 150 ms single-object targets. Flagged accordingly in every
experiment that uses them.

## Model shape

For a GET of `n` bytes: `sleep(TTFB_sample) ; stream at min(minio_rate,
cap)`. TTFB sampled as: p50 value for 99% of requests, long-tail value for
1% (deterministic hash of request id so runs are reproducible). No
per-request jitter beyond that — variance modeling can be added if a target
turns out to sit within the noise band.

Range GETs cost one TTFB each regardless of size — which is exactly the
physics the segment layout is designed against (few large ranges beat many
small ones).
