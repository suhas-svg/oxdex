//! `oxdex-tantivy-bench`: the same articles and queries through tantivy.
//!
//!     oxdex-tantivy-bench build --store data/out/simple --out data/tantivy/simple
//!     oxdex-tantivy-bench bench --index data/tantivy/simple --queries benches/queries-simple.txt
//!     oxdex-tantivy-bench eval  --index data/tantivy/simple --file benches/eval-simple.tsv
//!
//! Set up to match Oxdex as closely as tantivy allows: English stemming,
//! term frequencies but no positions, title and body as separate fields with
//! the title boosted 2x, AND by default with an OR fallback under 10 hits,
//! top 10 by BM25. Timings cover matching and ranking only (no stored-text
//! fetch or snippets), so they compare with Oxdex's `rank_ms`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use oxdex_docstore::DocStore;
use tantivy::collector::TopDocs;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{Field, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::{DocAddress, Index, IndexWriter, TantivyDocument, doc};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Build {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Indexing memory budget, split across tantivy's writer threads.
        #[arg(long, default_value_t = 1024)]
        mem_mb: usize,
    },
    Bench {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        queries: PathBuf,
        #[arg(long, default_value_t = 3)]
        rounds: usize,
    },
    Eval {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        file: PathBuf,
    },
}

fn schema() -> Schema {
    let mut b = Schema::builder();
    let indexing =
        TextFieldIndexing::default().set_tokenizer("en_stem").set_index_option(IndexRecordOption::WithFreqs);
    let text = TextOptions::default().set_indexing_options(indexing);
    b.add_text_field("title", text.clone() | STORED);
    b.add_text_field("body", text);
    b.build()
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build { store, out, mem_mb } => build(&store, &out, mem_mb),
        Cmd::Bench { index, queries, rounds } => bench(&index, &queries, rounds),
        Cmd::Eval { index, file } => eval(&index, &file),
    }
}

fn build(store: &Path, out: &Path, mem_mb: usize) -> Result<()> {
    let started = Instant::now();
    if out.exists() {
        std::fs::remove_dir_all(out).with_context(|| format!("clearing {}", out.display()))?;
    }
    std::fs::create_dir_all(out)?;
    let schema = schema();
    let (title, body) = (schema.get_field("title")?, schema.get_field("body")?);
    let index = Index::create_in_dir(out, schema)?;
    let mut writer: IndexWriter = index.writer(mem_mb * 1024 * 1024)?;
    // Background merges would race the explicit merge below.
    writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
    let ds = DocStore::open(store)?;
    let mut text_bytes = 0u64;
    for d in ds.iter() {
        let d = d?;
        text_bytes += (d.title.len() + d.text.len()) as u64;
        writer.add_document(doc!(title => d.title, body => d.text))?;
    }
    let add_secs = started.elapsed().as_secs_f64();
    writer.commit()?;
    let commit_secs = started.elapsed().as_secs_f64() - add_secs;
    // Merge to one segment, like Oxdex's merged index, so search is comparable.
    let ids = index.searchable_segment_ids()?;
    if ids.len() > 1 {
        writer.merge(&ids).wait()?;
    }
    writer.wait_merging_threads()?;
    let total = started.elapsed().as_secs_f64();

    let bytes = walk_size(out)?;
    println!(
        "tantivy build: {} docs, {total:.1} s total (add {add_secs:.1} s, commit {commit_secs:.1} s, merge {:.1} s), \
         index {:.1} MB = {:.1}% of {:.0} MB text (includes stored titles)",
        ds.num_docs(),
        total - add_secs - commit_secs,
        bytes as f64 / 1e6,
        100.0 * bytes as f64 / text_bytes as f64,
        text_bytes as f64 / 1e6
    );
    Ok(())
}

fn walk_size(dir: &Path) -> Result<u64> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let m = e.metadata()?;
        n += if m.is_dir() { walk_size(&e.path())? } else { m.len() };
    }
    Ok(n)
}

struct Engine {
    searcher: tantivy::Searcher,
    parser: QueryParser,
    title: Field,
}

impl Engine {
    fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir)?;
        let schema = index.schema();
        let (title, body) = (schema.get_field("title")?, schema.get_field("body")?);
        let reader = index.reader()?;
        let mut parser = QueryParser::for_index(&index, vec![title, body]);
        parser.set_conjunction_by_default();
        parser.set_field_boost(title, 2.0);
        Ok(Self { searcher: reader.searcher(), parser, title })
    }

    fn parse(&self, q: &str) -> Option<Box<dyn Query>> {
        self.parser.parse_query(q).ok()
    }

    /// Top 10, rerun as OR when AND finds fewer than 10 (as Oxdex does).
    fn top10(&self, q: &str) -> Result<Vec<DocAddress>> {
        let Some(query) = self.parse(q) else { return Ok(Vec::new()) };
        let mut top = self.searcher.search(&query, &TopDocs::with_limit(10))?;
        if top.len() < 10 {
            let any = q.split_whitespace().collect::<Vec<_>>().join(" OR ");
            if let Some(or) = self.parse(&any) {
                top = self.searcher.search(&or, &TopDocs::with_limit(10))?;
            }
        }
        Ok(top.into_iter().map(|(_, a)| a).collect())
    }

    fn title_of(&self, a: DocAddress) -> Result<String> {
        let d: TantivyDocument = self.searcher.doc(a)?;
        Ok(d.get_first(self.title).and_then(|v| v.as_str()).unwrap_or_default().to_owned())
    }
}

/// Replaces characters tantivy's query grammar treats as syntax, so plain
/// keyword queries mean the same thing to both engines.
fn plain(q: &str) -> String {
    q.chars().map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' }).collect()
}

fn bench(index: &Path, queries: &Path, rounds: usize) -> Result<()> {
    let e = Engine::open(index)?;
    let text = std::fs::read_to_string(queries)?;
    let qs: Vec<String> = text.lines().map(plain).filter(|l| !l.trim().is_empty()).collect();
    for round in 1..=rounds.max(1) {
        let mut lat: Vec<f64> = Vec::with_capacity(qs.len());
        let started = Instant::now();
        for q in &qs {
            let t = Instant::now();
            std::hint::black_box(e.top10(q)?);
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let secs = started.elapsed().as_secs_f64();
        lat.sort_by(f64::total_cmp);
        let pct = |p: f64| lat[(((p / 100.0) * lat.len() as f64).ceil() as usize).clamp(1, lat.len()) - 1];
        println!(
            "tantivy round {round}: {} queries in {secs:.2} s, ranking p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms",
            lat.len(),
            pct(50.0),
            pct(95.0),
            pct(99.0)
        );
    }
    Ok(())
}

fn eval(index: &Path, file: &Path) -> Result<()> {
    let e = Engine::open(index)?;
    let text = std::fs::read_to_string(file)?;
    let (mut s1, mut s3, mut s10, mut rr, mut n) = (0usize, 0usize, 0usize, 0.0f64, 0usize);
    for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let (q, want) = line.split_once('\t').context("no tab")?;
        let titles: Vec<String> = e.top10(&plain(q))?.into_iter().map(|a| e.title_of(a)).collect::<Result<_>>()?;
        if let Some(p) = titles.iter().position(|t| t == want) {
            s1 += usize::from(p < 1);
            s3 += usize::from(p < 3);
            s10 += 1;
            rr += 1.0 / (p + 1) as f64;
        }
        n += 1;
    }
    let pct = |x: usize| 100.0 * x as f64 / n as f64;
    println!(
        "tantivy eval: {n} queries: success@1 {:.0}%, success@3 {:.0}%, success@10 {:.0}%, MRR@10 {:.3}",
        pct(s1),
        pct(s3),
        pct(s10),
        rr / n as f64
    );
    Ok(())
}
