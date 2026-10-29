//! Closed-loop HTTP load generator: N workers send chat completions back to back for a duration
//! and report throughput, error counts and latency percentiles.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::json;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "loadgen",
    about = "Closed-loop load generator for OpenAI-compatible endpoints"
)]
struct Args {
    /// Full URL of the chat completions endpoint.
    #[arg(long, default_value = "http://127.0.0.1:4000/v1/chat/completions")]
    url: String,
    #[arg(long, default_value = "sk-master")]
    key: String,
    #[arg(long, default_value = "gpt-4o-mini")]
    model: String,
    #[arg(long, default_value_t = 64)]
    concurrency: usize,
    #[arg(long, default_value_t = 30)]
    seconds: u64,
    #[arg(long, default_value_t = false)]
    stream: bool,
    /// Warm-up seconds excluded from the statistics.
    #[arg(long, default_value_t = 3)]
    warmup: u64,
}

struct Stats {
    ok: AtomicU64,
    failed: AtomicU64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.concurrency * 2)
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client");
    let body = json!({
        "model": args.model,
        "stream": args.stream,
        "messages": [
            {"role": "system", "content": "You are a terse assistant."},
            {"role": "user", "content": "Write one sentence about the quick brown fox and the lazy dog, then stop. ".repeat(8)}
        ],
        "max_tokens": 200
    })
    .to_string();
    let stats = Arc::new(Stats {
        ok: AtomicU64::new(0),
        failed: AtomicU64::new(0),
    });
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    let measure_from = Instant::now() + Duration::from_secs(args.warmup);
    let mut workers = Vec::with_capacity(args.concurrency);
    for _ in 0..args.concurrency {
        let client = client.clone();
        let args = args.clone();
        let body = body.clone();
        let stats = stats.clone();
        workers.push(tokio::spawn(async move {
            let mut latencies: Vec<u64> = Vec::with_capacity(4096);
            while Instant::now() < deadline {
                let started = Instant::now();
                let result = client
                    .post(&args.url)
                    .bearer_auth(&args.key)
                    .header("content-type", "application/json")
                    .body(body.clone())
                    .send()
                    .await;
                let ok = match result {
                    Ok(response) => {
                        let status = response.status();
                        let drained = response.bytes().await.is_ok();
                        status.is_success() && drained
                    }
                    Err(_) => false,
                };
                let elapsed = started.elapsed().as_micros() as u64;
                if Instant::now() >= measure_from {
                    if ok {
                        stats.ok.fetch_add(1, Ordering::Relaxed);
                        latencies.push(elapsed);
                    } else {
                        stats.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            latencies
        }));
    }
    let mut all: Vec<u64> = Vec::new();
    for worker in workers {
        if let Ok(latencies) = worker.await {
            all.extend(latencies);
        }
    }
    all.sort_unstable();
    let measured = (args.seconds.saturating_sub(args.warmup)).max(1) as f64;
    let ok = stats.ok.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);
    let mean = if all.is_empty() {
        0.0
    } else {
        all.iter().sum::<u64>() as f64 / all.len() as f64 / 1000.0
    };
    println!(
        "{}",
        json!({
            "url": args.url,
            "concurrency": args.concurrency,
            "stream": args.stream,
            "seconds_measured": measured,
            "requests_ok": ok,
            "requests_failed": failed,
            "rps": (ok as f64 / measured).round(),
            "latency_ms": {
                "mean": (mean * 100.0).round() / 100.0,
                "p50": percentile(&all, 50.0) as f64 / 1000.0,
                "p90": percentile(&all, 90.0) as f64 / 1000.0,
                "p99": percentile(&all, 99.0) as f64 / 1000.0,
                "max": all.last().copied().unwrap_or(0) as f64 / 1000.0,
            }
        })
    );
}
