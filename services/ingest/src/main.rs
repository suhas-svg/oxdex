//! `oxdex-ingest`: turns a Wikipedia multistream dump into a document store.
//!
//!     oxdex-ingest build  --dump data/dumps/simplewiki-…-multistream.xml.bz2 --out data/out/simple
//!     oxdex-ingest sample --store data/out/simple -n 20
//!     oxdex-ingest show   --store data/out/simple 42

mod mem;

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bzip2::read::BzDecoder;
use clap::{Parser, Subcommand};
use memmap2::Mmap;
use oxdex_docstore::{DocStore, DocStoreWriter};
use oxdex_wikitext::{Page, StreamRange, clean, parse_index, parse_pages, parse_siteinfo, redirect_target, stream_ranges};
use rayon::prelude::*;
use serde::Serialize;
use tracing::{info, warn};

#[derive(Parser)]
#[command(version, about = "Wikipedia dump → Oxdex document store")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ingest a multistream dump into a document store.
    Build {
        /// The `…-pages-articles-multistream.xml.bz2` file.
        #[arg(long)]
        dump: PathBuf,
        /// Its `…-multistream-index.txt.bz2`. Default: derived from --dump.
        #[arg(long)]
        index: Option<PathBuf>,
        /// Output directory for the document store.
        #[arg(long)]
        out: PathBuf,
        /// Only process the first N page streams (~100 pages each).
        #[arg(long)]
        max_streams: Option<usize>,
        /// Streams handed to the thread pool per batch.
        #[arg(long, default_value_t = 512)]
        batch: usize,
        /// Overwrite an existing store in --out.
        #[arg(long)]
        force: bool,
    },
    /// Load an `id<TAB>text` file (e.g. MS MARCO's collection.tsv) into a
    /// document store. IDs must run 0, 1, 2, … in order, so doc ID = line ID.
    Tsv {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Used as each doc's URL: `<prefix><id>`.
        #[arg(long, default_value = "msmarco:")]
        url_prefix: String,
        #[arg(long)]
        force: bool,
    },
    /// Print N random articles to eyeball the cleaner.
    Sample {
        #[arg(long)]
        store: PathBuf,
        #[arg(short, default_value_t = 20)]
        n: usize,
        /// RNG seed (default: current time).
        #[arg(long)]
        seed: Option<u64>,
        /// Characters of text to print per article.
        #[arg(long, default_value_t = 700)]
        chars: usize,
    },
    /// Count articles that still contain leftover markup.
    Audit {
        #[arg(long)]
        store: PathBuf,
        /// Print up to this many example titles per marker.
        #[arg(long, default_value_t = 3)]
        examples: usize,
    },
    /// Print one article in full.
    Show {
        #[arg(long)]
        store: PathBuf,
        id: u32,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().cmd {
        Cmd::Build { dump, index, out, max_streams, batch, force } => {
            build(&dump, index, &out, max_streams, batch.max(1), force)
        }
        Cmd::Sample { store, n, seed, chars } => sample(&store, n, seed, chars),
        Cmd::Audit { store, examples } => audit(&store, examples),
        Cmd::Tsv { input, out, url_prefix, force } => tsv(&input, &out, &url_prefix, force),
        Cmd::Show { store, id } => show(&store, id),
    }
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

/// Pages skipped, by reason.
#[derive(Debug, Default, Clone, Copy, Serialize)]
struct Skips {
    /// Not namespace 0 (talk, user, template, category, …).
    non_article_namespace: u64,
    /// Redirect pages (recorded in redirects.tsv, not stored as articles).
    redirect: u64,
    /// Nothing left after cleaning (stubs that were all templates).
    empty_after_cleaning: u64,
    /// The cleaner panicked. Must stay 0.
    cleaner_panic: u64,
}

impl Skips {
    fn add(&mut self, o: &Skips) {
        self.non_article_namespace += o.non_article_namespace;
        self.redirect += o.redirect;
        self.empty_after_cleaning += o.empty_after_cleaning;
        self.cleaner_panic += o.cleaner_panic;
    }
}

struct Article {
    title: String,
    text: String,
    links: Vec<String>,
}

#[derive(Default)]
struct StreamOutput {
    articles: Vec<Article>,
    redirects: Vec<(String, String)>,
    skips: Skips,
    pages_seen: u64,
    error: Option<String>,
}

#[derive(Serialize)]
struct Meta {
    format_version: u32,
    source_dump: String,
    sitename: String,
    article_url_prefix: String,
    num_docs: u32,
    num_blocks: u32,
    num_links: u64,
    num_redirects: u64,
    pages_seen: u64,
    skipped: Skips,
    bad_streams: u64,
    raw_text_bytes: u64,
    docs_bin_bytes: u64,
    streams_processed: usize,
    elapsed_secs: f64,
    built_unix_secs: u64,
}

fn build(
    dump: &Path,
    index: Option<PathBuf>,
    out: &Path,
    max_streams: Option<usize>,
    batch: usize,
    force: bool,
) -> Result<()> {
    let started = Instant::now();
    if out.join("docs.idx").exists() && !force {
        bail!("{} already holds a document store (pass --force to overwrite)", out.display());
    }
    let index = match index {
        Some(p) => p,
        None => derive_index_path(dump)?,
    };

    let file = File::open(dump).with_context(|| format!("opening {}", dump.display()))?;
    // SAFETY: the dump is read-only input and is not modified while we run.
    let mmap = unsafe { Mmap::map(&file) }.context("memory-mapping the dump")?;
    let file_len = mmap.len() as u64;

    let offsets = parse_index(BufReader::new(
        File::open(&index).with_context(|| format!("opening {}", index.display()))?,
    ))
    .with_context(|| format!("reading {}", index.display()))?;
    let ranges = stream_ranges(&offsets, file_len);
    let (header, mut streams): (Option<StreamRange>, Vec<StreamRange>) =
        if offsets.first().is_some_and(|&o| o > 0) {
            (ranges.first().copied(), ranges[1..].to_vec())
        } else {
            (None, ranges)
        };
    if let Some(max) = max_streams {
        streams.truncate(max);
    }

    let site = match header {
        Some(h) => parse_siteinfo(BufReader::new(BzDecoder::new(slice(&mmap, h))))
            .context("reading <siteinfo> from the dump header")?,
        None => Default::default(),
    };
    info!(
        dump = %dump.display(),
        dump_mb = file_len / 1_000_000,
        streams = streams.len(),
        threads = rayon::current_num_threads(),
        site = %site.article_url_prefix,
        "starting ingest"
    );

    std::fs::create_dir_all(out)?;
    let mut writer = DocStoreWriter::create(out)?;
    let redirects_path = out.join("redirects.tsv");
    let mut redirects = BufWriter::new(File::create(&redirects_path)?);

    let mut skips = Skips::default();
    let mut pages_seen = 0u64;
    let mut num_redirects = 0u64;
    let mut bad_streams = 0u64;
    let mut done = 0usize;
    let mut last_log = Instant::now();

    for chunk in streams.chunks(batch) {
        let outputs: Vec<StreamOutput> =
            chunk.par_iter().map(|r| process_stream(slice(&mmap, *r))).collect();

        // Written in stream order, so doc IDs are deterministic.
        for (range, o) in chunk.iter().zip(outputs) {
            if let Some(err) = &o.error {
                bad_streams += 1;
                warn!(offset = range.start, error = %err, kept_pages = o.pages_seen, "bad stream");
            }
            for a in &o.articles {
                let url = format!("{}{}", site.article_url_prefix, a.title.replace(' ', "_"));
                writer.add(&a.title, &url, &a.text, &a.links)?;
            }
            for (from, to) in &o.redirects {
                writeln!(redirects, "{from}\t{to}")?;
            }
            num_redirects += o.redirects.len() as u64;
            skips.add(&o.skips);
            pages_seen += o.pages_seen;
        }

        done += chunk.len();
        if last_log.elapsed().as_secs() >= 5 || done == streams.len() {
            last_log = Instant::now();
            let secs = started.elapsed().as_secs_f64();
            let bytes_done = chunk.last().map_or(0, |r| r.end);
            let (rss, peak) = mem::rss().unwrap_or((0, 0));
            info!(
                streams = format!("{done}/{}", streams.len()),
                pages = pages_seen,
                mb_per_s = format!("{:.1}", bytes_done as f64 / 1e6 / secs),
                rss_mb = mem::mb(rss),
                peak_rss_mb = mem::mb(peak),
                elapsed_s = format!("{secs:.0}"),
                "progress"
            );
        }
    }

    redirects.flush()?;
    let summary = writer.finish()?;
    let elapsed = started.elapsed().as_secs_f64();

    info!(
        non_article_namespace = skips.non_article_namespace,
        redirect = skips.redirect,
        empty_after_cleaning = skips.empty_after_cleaning,
        cleaner_panic = skips.cleaner_panic,
        bad_streams,
        "skipped pages by reason"
    );
    info!(
        docs = summary.num_docs,
        links = summary.num_links,
        redirects = num_redirects,
        raw_text_mb = summary.raw_bytes / 1_000_000,
        docs_bin_mb = summary.docs_bytes / 1_000_000,
        peak_rss_mb = mem::mb(mem::rss().map_or(0, |(_, peak)| peak)),
        elapsed_s = format!("{elapsed:.1}"),
        "ingest done"
    );

    let meta = Meta {
        format_version: oxdex_docstore::FORMAT_VERSION,
        source_dump: dump.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
        sitename: site.sitename,
        article_url_prefix: site.article_url_prefix,
        num_docs: summary.num_docs,
        num_blocks: summary.num_blocks,
        num_links: summary.num_links,
        num_redirects,
        pages_seen,
        skipped: skips,
        bad_streams,
        raw_text_bytes: summary.raw_bytes,
        docs_bin_bytes: summary.docs_bytes,
        streams_processed: streams.len(),
        elapsed_secs: elapsed,
        built_unix_secs: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    };
    std::fs::write(out.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;

    if skips.cleaner_panic > 0 || bad_streams > 0 {
        warn!("finished with {} cleaner panics and {bad_streams} bad streams", skips.cleaner_panic);
    }
    Ok(())
}

fn slice(mmap: &Mmap, r: StreamRange) -> &[u8] {
    &mmap[r.start as usize..r.end as usize]
}

fn derive_index_path(dump: &Path) -> Result<PathBuf> {
    let name = dump.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(stem) = name.strip_suffix(".xml.bz2") else {
        bail!("cannot derive the index file name from {name}; pass --index");
    };
    Ok(dump.with_file_name(format!("{stem}-index.txt.bz2")))
}

/// Decompresses and parses one ~100-page stream. Runs on a worker thread.
fn process_stream(compressed: &[u8]) -> StreamOutput {
    let mut out = StreamOutput::default();
    let result = parse_pages(BufReader::new(BzDecoder::new(compressed)), |page| {
        out.pages_seen += 1;
        handle_page(page, &mut out);
    });
    if let Err(e) = result {
        out.error = Some(e.to_string());
    }
    out
}

fn handle_page(page: Page, out: &mut StreamOutput) {
    if page.ns != 0 {
        out.skips.non_article_namespace += 1;
        return;
    }
    let target = page
        .redirect
        .as_deref()
        .map(oxdex_wikitext::normalize_title)
        .or_else(|| redirect_target(&page.text));
    if let Some(target) = target {
        out.skips.redirect += 1;
        out.redirects.push((page.title, target));
        return;
    }

    let cleaned = match catch_unwind(AssertUnwindSafe(|| clean(&page.text))) {
        Ok(c) => c,
        Err(_) => {
            out.skips.cleaner_panic += 1;
            warn!(title = %page.title, id = page.id, "cleaner panicked");
            return;
        }
    };
    if cleaned.text.is_empty() {
        out.skips.empty_after_cleaning += 1;
        return;
    }
    out.articles.push(Article {
        title: page.title,
        text: cleaned.text,
        links: cleaned.links,
    });
}

// ---------------------------------------------------------------------------
// sample / show
// ---------------------------------------------------------------------------

fn sample(store: &Path, n: usize, seed: Option<u64>, chars: usize) -> Result<()> {
    let ds = DocStore::open(store)?;
    if ds.num_docs() == 0 {
        bail!("store is empty");
    }
    let mut x = seed.unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64)
    }) | 1;
    for _ in 0..n {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let id = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) % u64::from(ds.num_docs())) as u32;
        let d = ds.get(id)?;
        let preview: String = d.text.chars().take(chars).collect();
        println!("══ [{}] {}  ({} chars)\n{}\n", d.id, d.title, d.text.chars().count(), d.url);
        println!("{preview}{}\n", if d.text.chars().count() > chars { " …" } else { "" });
    }
    Ok(())
}

/// Markup that should not survive cleaning.
const AUDIT_MARKERS: &[&str] = &[
    "{{", "}}", "[[", "]]", "{|", "|}", "<ref", "</", "/>", "''", "&nbsp;", "&amp;", "__",
    "thumb|", "px|", "File:", "Category:", "http://", "https://", "==",
];

fn audit(store: &Path, examples: usize) -> Result<()> {
    let ds = DocStore::open(store)?;
    let mut hits = vec![0u64; AUDIT_MARKERS.len()];
    let mut seen: Vec<Vec<String>> = vec![Vec::new(); AUDIT_MARKERS.len()];
    let mut dirty = 0u64;
    for doc in ds.iter() {
        let doc = doc?;
        let mut any = false;
        for (i, m) in AUDIT_MARKERS.iter().enumerate() {
            if doc.text.contains(m) {
                hits[i] += 1;
                any = true;
                if seen[i].len() < examples {
                    seen[i].push(format!("[{}] {}", doc.id, doc.title));
                }
            }
        }
        dirty += u64::from(any);
    }
    let n = f64::from(ds.num_docs().max(1));
    println!("{:<10} {:>8} {:>8}  examples", "marker", "docs", "%");
    for (i, m) in AUDIT_MARKERS.iter().enumerate() {
        println!("{:<10} {:>8} {:>7.3}%  {}", format!("{m:?}"), hits[i], hits[i] as f64 / n * 100.0, seen[i].join(" · "));
    }
    println!(
        "\n{dirty} of {} docs ({:.2}%) contain at least one marker; {:.2}% are clean.",
        ds.num_docs(),
        dirty as f64 / n * 100.0,
        100.0 - dirty as f64 / n * 100.0
    );
    Ok(())
}

fn show(store: &Path, id: u32) -> Result<()> {
    let d = DocStore::open(store)?.get(id)?;
    println!("[{}] {}\n{}\n\n{}", d.id, d.title, d.url, d.text);
    Ok(())
}

fn tsv(input: &Path, out: &Path, url_prefix: &str, force: bool) -> Result<()> {
    use std::io::BufRead;
    let started = Instant::now();
    if out.join("docs.idx").exists() && !force {
        bail!("{} already holds a store; pass --force to overwrite", out.display());
    }
    std::fs::create_dir_all(out)?;
    let mut w = DocStoreWriter::create(out)?;
    let reader = BufReader::with_capacity(4 << 20, File::open(input).with_context(|| format!("opening {}", input.display()))?);
    let mut n: u64 = 0;
    let mut last_log = Instant::now();
    for line in reader.lines() {
        let line = line?;
        let (id, text) = line.split_once('\t').with_context(|| format!("line {n}: no tab"))?;
        let id: u64 = id.trim().parse().with_context(|| format!("line {n}: bad id {id:?}"))?;
        if id != n {
            bail!("line {n} has id {id}; ids must run 0, 1, 2, … so doc IDs match them");
        }
        w.add("", &format!("{url_prefix}{id}"), text, &[])?;
        n += 1;
        if last_log.elapsed().as_secs() >= 10 {
            last_log = Instant::now();
            info!(docs = n, elapsed_s = format!("{:.0}", started.elapsed().as_secs_f64()), "loading");
        }
    }
    let summary = w.finish()?;
    info!(
        docs = summary.num_docs,
        raw_mb = summary.raw_bytes / 1_000_000,
        docs_mb = summary.docs_bytes / 1_000_000,
        secs = format!("{:.1}", started.elapsed().as_secs_f64()),
        "tsv store done"
    );
    Ok(())
}
