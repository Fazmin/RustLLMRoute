# Performance

Measured on 2026-10-28 on a Windows 11 developer machine (release build, `lto = "fat"`), all
processes on the same host over loopback. The tools live in `crates/rustllmroute-testkit`:

```sh
cargo build --release -p rustllmroute-server -p rustllmroute-testkit
target/release/fake-upstream --bind 127.0.0.1:18080 --latency-ms 20      # canned OpenAI replies
target/release/rustllmroute --config config.yaml --port 4000 --log-level warn
target/release/loadgen --url http://127.0.0.1:4000/v1/chat/completions --key sk-... \
  --model gpt-4o-mini --concurrency 64 --seconds 20 [--stream]
```

`loadgen` is closed-loop: 64 workers send a 4 KB chat request back to back and record the time
to the last byte. The fake upstream answers after a fixed 20 ms, so with 64 workers the ceiling is
about 3 200 requests/s and the measured rate is dominated by the client/server pair, not the
gateway. The interesting number is the latency the gateway adds.

## Added latency (p99, 64 concurrent, 20 ms upstream)

| Path | mean | p50 | p90 | p99 | max | req/s | failed |
|---|---|---|---|---|---|---|---|
| direct to upstream, non-stream | 31.20 | 31.19 | 32.10 | 32.85 | 33.9 | 2055 | 0 |
| gateway, master key only, non-stream | 31.17 | 31.17 | 32.46 | 33.91 | 36.4 | 2057 | 0 |
| gateway with SQLite spend logging, non-stream | 31.19 | 31.19 | 32.75 | 34.17 | 36.9 | 2056 | 0 |
| direct to upstream, stream (21 chunks) | 31.20 | 31.20 | 32.13 | 33.19 | 34.3 | 2056 | 0 |
| gateway, stream | 31.19 | 31.21 | 32.96 | 34.60 | 42.2 | 2056 | 0 |

Gateway overhead at p99: about 1.1 ms without a database, 1.3 ms with SQLite spend logging,
1.4 ms for streams; mean overhead is below the measurement noise. This meets the plan's target
of under 2 ms added p99 at about 2 000 requests/s (the target was stated for 1 000 requests/s).

## Soak (SQLite, 64 concurrent)

| | value |
|---|---|
| duration | 20 s warm-up run, then 90 s soak, then 20 s streaming |
| requests | 178 624 in the 90 s soak, 0 failed, p50 31.2 ms, p99 33.6 ms |
| spend rows persisted | 266 944 across the runs, `spend_events_dropped` 0 |
| RSS idle | 73 MB |
| RSS after the 20 s run | 97 MB |
| RSS after the 90 s soak | 106 MB |
| RSS after the streaming run | 110 MB |
| gateway errors logged | 0 |

Memory grows quickly during the first seconds (SQLite page cache, auth caches, Prometheus
series, allocator arenas) and then slowly: 10 MB over the 90 s soak and 4 MB over the following
streaming run. A 10-minute soak to confirm the curve flattens, as the plan asks, has not been run
yet; the numbers above are from a 2.5-minute session. Everything recorded here was produced by
the scripts in the repository, so re-running them on the target hardware is the right way to get
figures for a specific deployment.
