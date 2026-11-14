//! `oxdex-search-api`: the Oxdex HTTP service and web page.
//!
//!     oxdex-search-api --index data/index/simple --store data/out/simple
//!     oxdex-search-api bench --index … --store … --queries benches/queries-simple.txt
//!
//! The server binds first and loads the index in the background, answering
//! 503 until it is ready (memory mapping makes that near-instant).

mod app;
mod engine;
mod latency;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{error, info};

use crate::app::{AppState, router};
use crate::engine::Engine;
use crate::latency::Latencies;

#[derive(Parser)]
#[command(version, about = "Oxdex search service")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[arg(long, default_value = "data/index/simple")]
    index: PathBuf,
    #[arg(long, default_value = "data/out/simple")]
    store: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8080")]
    addr: SocketAddr,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a query file through the engine in-process and report latency.
    Bench {
        /// One query per line.
        #[arg(long)]
        queries: PathBuf,
        #[arg(short, default_value_t = 10)]
        k: usize,
        /// Passes over the file; the first warms the page cache.
        #[arg(long, default_value_t = 2)]
        rounds: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();

    if let Some(Cmd::Bench { queries, k, rounds }) = cli.cmd {
        return bench(&cli.index, &cli.store, &queries, k, rounds);
    }

    let state = AppState::new();
    let (index, store, cell) = (cli.index.clone(), cli.store.clone(), state.engine.clone());
    let load_started = Instant::now();
    tokio::task::spawn_blocking(move || match Engine::open(&index, &store) {
        Ok(engine) => {
            info!(
                docs = engine.body.meta().num_docs,
                terms = engine.body.meta().num_terms,
                title_field = engine.title.is_some(),
                ms = format!("{:.1}", load_started.elapsed().as_secs_f64() * 1e3),
                "index loaded"
            );
            let _ = cell.set(Arc::new(engine));
        }
        Err(e) => {
            error!("failed to load the index: {e:#}");
            std::process::exit(1);
        }
    });

    let listener = tokio::net::TcpListener::bind(cli.addr)
        .await
        .with_context(|| format!("binding {}", cli.addr))?;
    info!("listening on http://{}", cli.addr);
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn bench(index: &Path, store: &Path, queries: &Path, k: usize, rounds: usize) -> Result<()> {
    let engine = Engine::open(index, store)?;
    let text = std::fs::read_to_string(queries).with_context(|| format!("reading {}", queries.display()))?;
    let qs: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    for round in 1..=rounds.max(1) {
        let mut lat = Latencies::new(qs.len());
        let mut hits = 0usize;
        let started = Instant::now();
        for q in &qs {
            let t = Instant::now();
            let r = engine.search(q, k, 0)?;
            lat.record(t.elapsed().as_secs_f64() * 1e3);
            hits += r.results.len();
        }
        let secs = started.elapsed().as_secs_f64();
        println!(
            "round {round}: {} queries in {secs:.2} s ({:.0} q/s single-threaded), p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms, {:.1} results/query",
            qs.len(),
            qs.len() as f64 / secs,
            lat.percentile(50.0).unwrap_or(0.0),
            lat.percentile(95.0).unwrap_or(0.0),
            lat.percentile(99.0).unwrap_or(0.0),
            lat.percentile(100.0).unwrap_or(0.0),
            hits as f64 / qs.len().max(1) as f64,
        );
    }
    Ok(())
}
