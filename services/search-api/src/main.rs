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
    /// Searches allowed to run at once; more wait in line. Default: one per core.
    #[arg(long)]
    search_slots: Option<usize>,
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
        #[command(flatten)]
        tune: Tune,
    },
    /// MS MARCO passage ranking: MRR@10 over the dev queries. The index must
    /// be built from collection.tsv, so doc ID = passage ID.
    Msmarco {
        #[arg(long, default_value = "data/dumps/msmarco/queries.dev.tsv")]
        queries: PathBuf,
        #[arg(long, default_value = "data/dumps/msmarco/qrels.dev.small.tsv")]
        qrels: PathBuf,
        /// Score queries as OR (the standard BM25 baseline setup) instead of
        /// Oxdex's AND-then-OR.
        #[arg(long)]
        or: bool,
        #[command(flatten)]
        tune: Tune,
    },
    /// Relevance: success@1/3/10 and MRR@10 for a `query<TAB>expected title` file.
    Eval {
        #[arg(long, default_value = "benches/eval-simple.tsv")]
        file: PathBuf,
        /// Print every query's rank, not only the misses.
        #[arg(long)]
        verbose: bool,
        #[command(flatten)]
        tune: Tune,
    },
}

/// Ranking knobs; unset ones keep the engine defaults.
#[derive(clap::Args, Clone, Copy, Default)]
struct Tune {
    #[arg(long)]
    k1: Option<f64>,
    #[arg(long)]
    body_b: Option<f64>,
    #[arg(long)]
    title_k1: Option<f64>,
    #[arg(long)]
    title_b: Option<f64>,
    #[arg(long)]
    title_boost: Option<f64>,
    /// Score every OR match instead of using WAND.
    #[arg(long)]
    no_wand: bool,
}

impl Tune {
    fn apply(self, p: &mut oxdex_query::Params) {
        if let Some(v) = self.k1 { p.bm25.k1 = v; }
        if let Some(v) = self.body_b { p.bm25.b = v; }
        if let Some(v) = self.title_k1 { p.title_bm25.k1 = v; }
        if let Some(v) = self.title_b { p.title_bm25.b = v; }
        if let Some(v) = self.title_boost { p.title_boost = v; }
        if self.no_wand { p.wand = false; }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();

    match cli.cmd {
        Some(Cmd::Bench { queries, k, rounds, tune }) => return bench(&cli.index, &cli.store, &queries, k, rounds, tune),
        Some(Cmd::Eval { file, verbose, tune }) => return eval(&cli.index, &cli.store, &file, verbose, tune),
        Some(Cmd::Msmarco { queries, qrels, or, tune }) => {
            return msmarco(&cli.index, &cli.store, &queries, &qrels, or, tune);
        }
        None => {}
    }

    let state = match cli.search_slots {
        Some(n) => AppState::with_search_slots(n),
        None => AppState::new(),
    };
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

fn bench(index: &Path, store: &Path, queries: &Path, k: usize, rounds: usize, tune: Tune) -> Result<()> {
    let mut engine = Engine::open(index, store)?;
    tune.apply(&mut engine.params);
    let text = std::fs::read_to_string(queries).with_context(|| format!("reading {}", queries.display()))?;
    let qs: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    for round in 1..=rounds.max(1) {
        let mut lat = Latencies::new(qs.len());
        let mut rank = Latencies::new(qs.len());
        let mut fetch = Latencies::new(qs.len());
        let mut modes = std::collections::BTreeMap::<&str, usize>::new();
        let mut hits = 0usize;
        let started = Instant::now();
        for q in &qs {
            let t = Instant::now();
            let r = engine.search(q, k, 0)?;
            lat.record(t.elapsed().as_secs_f64() * 1e3);
            rank.record(r.rank_ms);
            fetch.record(r.fetch_ms);
            *modes.entry(r.mode).or_default() += 1;
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
        println!(
            "         doc fetch: p50 {:.2} ms, p95 {:.2} ms (snippets are the rest)",
            fetch.percentile(50.0).unwrap_or(0.0),
            fetch.percentile(95.0).unwrap_or(0.0),
        );
        println!(
            "         ranking only: p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms; modes {modes:?}",
            rank.percentile(50.0).unwrap_or(0.0),
            rank.percentile(95.0).unwrap_or(0.0),
            rank.percentile(99.0).unwrap_or(0.0),
        );
    }
    Ok(())
}

fn eval(index: &Path, store: &Path, file: &Path, verbose: bool, tune: Tune) -> Result<()> {
    let mut engine = Engine::open(index, store)?;
    tune.apply(&mut engine.params);
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let cases: Vec<(&str, &str)> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split_once('\t').with_context(|| format!("no tab in line {l:?}")))
        .collect::<Result<_>>()?;

    // Every expected title must exist, or a typo would count as a ranking miss.
    let wanted: std::collections::HashSet<&str> = cases.iter().map(|c| c.1).collect();
    let mut found = std::collections::HashSet::new();
    for d in engine.store.iter() {
        let d = d?;
        if wanted.contains(d.title.as_str()) {
            found.insert(d.title);
        }
    }
    let missing: Vec<&&str> = wanted.iter().filter(|t| !found.contains(**t)).collect();
    anyhow::ensure!(missing.is_empty(), "expected titles not in the store: {missing:?}");

    let (mut s1, mut s3, mut s10, mut rr) = (0usize, 0usize, 0usize, 0.0f64);
    for (q, want) in &cases {
        let r = engine.search(q, 10, 0)?;
        let rank = r.results.iter().position(|x| x.title == *want).map(|p| p + 1);
        if let Some(n) = rank {
            s1 += usize::from(n <= 1);
            s3 += usize::from(n <= 3);
            s10 += 1;
            rr += 1.0 / n as f64;
        }
        if verbose || rank.is_none_or(|n| n > 3) {
            let top: Vec<&str> = r.results.iter().take(3).map(|x| x.title.as_str()).collect();
            println!(
                "{:>4}  {q:<48} want {want:?}  top3 {top:?}",
                rank.map_or("-".into(), |n| n.to_string())
            );
        }
    }
    let n = cases.len() as f64;
    println!(
        "{} queries: success@1 {:.0}%, success@3 {:.0}%, success@10 {:.0}%, MRR@10 {:.3}",
        cases.len(),
        100.0 * s1 as f64 / n,
        100.0 * s3 as f64 / n,
        100.0 * s10 as f64 / n,
        rr / n
    );
    Ok(())
}

fn msmarco(index: &Path, store: &Path, queries: &Path, qrels: &Path, or: bool, tune: Tune) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    let mut engine = Engine::open(index, store)?;
    tune.apply(&mut engine.params);

    // qrels: qid 0 pid 1
    let mut relevant: HashMap<u64, HashSet<u32>> = HashMap::new();
    for line in std::fs::read_to_string(qrels).with_context(|| format!("reading {}", qrels.display()))?.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 4 && f[3] != "0" {
            relevant.entry(f[0].parse()?).or_default().insert(f[2].parse()?);
        }
    }
    let text = std::fs::read_to_string(queries).with_context(|| format!("reading {}", queries.display()))?;
    let qs: Vec<(u64, &str)> = text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter_map(|(id, q)| Some((id.parse().ok()?, q)))
        .filter(|(id, _)| relevant.contains_key(id))
        .collect();

    let started = Instant::now();
    let mut lat = Latencies::new(qs.len());
    let (mut rr, mut found10, mut empty) = (0.0f64, 0usize, 0usize);
    for (qid, q) in &qs {
        let t = Instant::now();
        let ranked = engine.rank(q, 10, or);
        lat.record(t.elapsed().as_secs_f64() * 1e3);
        empty += usize::from(ranked.is_empty());
        let rel = &relevant[qid];
        if let Some(p) = ranked.iter().position(|d| rel.contains(d)) {
            rr += 1.0 / (p + 1) as f64;
            found10 += 1;
        }
    }
    let n = qs.len().max(1) as f64;
    println!(
        "MS MARCO dev: {} queries ({}), MRR@10 {:.4}, recall@10 {:.1}%, {} with no results; \
         ranking p50 {:.2} ms, p95 {:.2} ms, {:.0} s total",
        qs.len(),
        if or { "OR" } else { "AND, OR fallback" },
        rr / n,
        100.0 * found10 as f64 / n,
        empty,
        lat.percentile(50.0).unwrap_or(0.0),
        lat.percentile(95.0).unwrap_or(0.0),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
