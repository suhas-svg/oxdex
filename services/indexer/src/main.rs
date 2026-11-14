//! `oxdex-indexer`: builds, checks and searches the on-disk index.
//!
//!     oxdex-indexer build  --store data/out/simple --out data/index/simple
//!     oxdex-indexer check  --index data/index/simple
//!     oxdex-indexer search --index data/index/simple --store data/out/simple fpga timing

mod mem;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use oxdex_docstore::{Doc, DocStore};
use oxdex_index::{DiskIndex, IndexBuilder, check, count_terms};
use oxdex_query::{Bm25, search};
use rayon::prelude::*;
use tracing::{info, warn};

#[derive(Parser)]
#[command(version, about = "Oxdex index builder, checker and search")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build an index from a document store.
    Build {
        #[arg(long)]
        store: PathBuf,
        /// Output directory (a new one per build; never overwrite a served index).
        #[arg(long)]
        out: PathBuf,
        /// Segment buffer budget: flush a segment when postings reach this size.
        #[arg(long, default_value_t = 1024)]
        mem_mb: usize,
        /// Documents tokenized in parallel per batch.
        #[arg(long, default_value_t = 4096)]
        batch: usize,
        /// Replace an existing index in --out.
        #[arg(long)]
        force: bool,
    },
    /// Decode every posting list and cross-check totals.
    Check {
        #[arg(long)]
        index: PathBuf,
    },
    /// Search the index. Omit the query to read queries from stdin.
    Search {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        store: PathBuf,
        query: Vec<String>,
        #[arg(short, default_value_t = 10)]
        k: usize,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(io::stderr)
        .init();

    match Cli::parse().cmd {
        Cmd::Build { store, out, mem_mb, batch, force } => build(&store, &out, mem_mb, batch.max(1), force),
        Cmd::Check { index } => run_check(&index),
        Cmd::Search { index, store, query, k } => run_search(&index, &store, &query.join(" "), k),
    }
}

fn rss_fields() -> (u64, u64) {
    mem::rss().map_or((0, 0), |(cur, peak)| (mem::mb(cur), mem::mb(peak)))
}

fn build(store: &Path, out: &Path, mem_mb: usize, batch: usize, force: bool) -> Result<()> {
    let started = Instant::now();
    if out.join("meta.json").exists() && !force {
        bail!("{} already holds an index (pass --force, or build into a new folder)", out.display());
    }
    let ds = DocStore::open(store).with_context(|| format!("opening document store {}", store.display()))?;
    let total = ds.num_docs();
    info!(store = %store.display(), docs = total, mem_mb, batch, threads = rayon::current_num_threads(), "starting index build");

    // Title field: each article's title plus every redirect pointing at it,
    // so "USA" finds "United States". It lives in <out>/title/.
    let redirects = load_redirects(store)?;
    let mut builder = IndexBuilder::create(out, mem_mb * 1024 * 1024)?;
    let mut title_builder = IndexBuilder::create(out.join("title"), (mem_mb / 4).max(64) * 1024 * 1024)?;
    let mut docs_iter = ds.iter();
    let mut last_log = Instant::now();
    let mut batch_docs: Vec<Doc> = Vec::with_capacity(batch);

    loop {
        batch_docs.clear();
        for d in docs_iter.by_ref().take(batch) {
            batch_docs.push(d?);
        }
        if batch_docs.is_empty() {
            break;
        }
        let analyzed: Vec<_> = batch_docs
            .par_iter()
            .map(|d| {
                let body = count_terms(&d.title, &d.text);
                let title_text = match redirects.get(d.title.as_str()) {
                    Some(alts) => format!("{}\n{}", d.title, alts.join("\n")),
                    None => d.title.clone(),
                };
                (body, count_terms(&title_text, ""))
            })
            .collect();
        for (d, ((terms, len), (tterms, tlen))) in batch_docs.iter().zip(analyzed) {
            let id = builder.add_doc(&terms, len)?;
            if id != d.id {
                bail!("doc store gave doc {} where {id} was expected", d.id);
            }
            title_builder.add_doc(&tterms, tlen)?;
        }

        if last_log.elapsed().as_secs() >= 10 {
            last_log = Instant::now();
            let done = builder.num_docs();
            let (rss, peak) = rss_fields();
            info!(
                docs = format!("{done}/{total}"),
                pct = format!("{:.1}", done as f64 / f64::from(total.max(1)) * 100.0),
                segments = builder.num_segments(),
                buffer_mb = builder.buffer_bytes() / (1024 * 1024),
                rss_mb = rss,
                peak_rss_mb = peak,
                elapsed_s = format!("{:.0}", started.elapsed().as_secs_f64()),
                "indexing"
            );
        }
    }

    let tokenize_secs = started.elapsed().as_secs_f64();
    drop(redirects);
    info!(segments = builder.num_segments() + 1, "all docs added; merging segments");
    let tstats = title_builder.finish()?;
    info!(
        terms = tstats.num_terms,
        segments = tstats.num_segments,
        index_mb = (tstats.postings_bytes + tstats.dict_bytes + tstats.doclens_bytes) / 1_000_000,
        "title field done"
    );
    let stats = builder.finish()?;
    let (_, peak) = rss_fields();
    info!(
        docs = stats.num_docs,
        terms = stats.num_terms,
        tokens = stats.total_tokens,
        segments = stats.num_segments,
        postings_mb = stats.postings_bytes / 1_000_000,
        dict_mb = stats.dict_bytes / 1_000_000,
        doclens_mb = stats.doclens_bytes / 1_000_000,
        peak_buffer_mb = stats.peak_buffer_bytes / (1024 * 1024),
        peak_rss_mb = peak,
        add_docs_s = format!("{tokenize_secs:.1}"),
        finish_s = format!("{:.1}", started.elapsed().as_secs_f64() - tokenize_secs),
        segment_writes_s = format!("{:.1}", stats.flush_secs),
        merge_s = format!("{:.1}", stats.merge_secs),
        total_s = format!("{:.1}", started.elapsed().as_secs_f64()),
        "index build done"
    );
    Ok(())
}

/// Reads `redirects.tsv` (redirect title → target title) into target → redirects.
/// Redirects whose target is itself missing simply never match a doc.
fn load_redirects(store: &Path) -> Result<HashMap<Box<str>, Vec<Box<str>>>> {
    let path = store.join("redirects.tsv");
    let mut map: HashMap<Box<str>, Vec<Box<str>>> = HashMap::new();
    if !path.exists() {
        warn!(path = %path.display(), "no redirects file; title field has titles only");
        return Ok(map);
    }
    let file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut n = 0u64;
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        if let Some((from, to)) = line.split_once('\t')
            && !from.is_empty()
            && from != to
        {
            map.entry(to.into()).or_default().push(from.into());
            n += 1;
        }
    }
    info!(redirects = n, targets = map.len(), "loaded redirects");
    Ok(map)
}

fn run_check(dir: &Path) -> Result<()> {
    for sub in [dir.to_path_buf(), dir.join("title")] {
        if sub.join("meta.json").exists() {
            check_one(&sub)?;
        }
    }
    Ok(())
}

fn check_one(dir: &Path) -> Result<()> {
    let started = Instant::now();
    let index = DiskIndex::open(dir)?;
    info!(index = %dir.display(), "check");
    info!(terms = index.num_terms(), docs = index.meta().num_docs, "checking every posting list");
    let report = check(&index, |n| info!(terms_checked = n, "checking"));
    info!(
        terms = report.num_terms,
        postings = report.num_postings,
        max_df = report.max_df,
        max_df_term = %report.max_df_term,
        errors = report.total_errors,
        elapsed_s = format!("{:.1}", started.elapsed().as_secs_f64()),
        "check done"
    );
    for e in &report.errors {
        warn!("{e}");
    }
    if !report.ok() {
        bail!("index check failed with {} errors", report.total_errors);
    }
    println!("OK: {} terms, {} postings, every list sorted and decodes cleanly.", report.num_terms, report.num_postings);
    Ok(())
}

fn run_search(index_dir: &Path, store_dir: &Path, query: &str, k: usize) -> Result<()> {
    let opened = Instant::now();
    let index = DiskIndex::open(index_dir)?;
    let store = DocStore::open(store_dir)?;
    if store.num_docs() != index.meta().num_docs {
        bail!("index has {} docs but the store has {}", index.meta().num_docs, store.num_docs());
    }
    eprintln!("opened in {:.1} ms", opened.elapsed().as_secs_f64() * 1e3);

    let bm25 = Bm25::default();
    let run = |q: &str| -> Result<()> {
        let t = Instant::now();
        let r = search(&index, q, k, &bm25);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!("{} results in {ms:.2} ms", r.total_hits);
        for (rank, hit) in r.hits.iter().enumerate() {
            let d = store.get(hit.doc)?;
            let preview: String = d.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(100).collect();
            println!("{:>2}. {:>7.3}  {}\n             {preview}…", rank + 1, hit.score, d.title);
        }
        Ok(())
    };

    if !query.trim().is_empty() {
        return run(query);
    }
    let stdin = io::stdin();
    loop {
        eprint!("> ");
        io::stderr().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        if !line.trim().is_empty() {
            run(line.trim())?;
        }
    }
}
