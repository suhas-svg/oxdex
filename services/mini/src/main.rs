//! M1 mini engine: index a folder of `.txt` files in memory and search it.
//!
//! Each file's first non-empty line is its title; the rest is its body.
//!
//!     oxdex-mini data/mini fpga timing     # one query
//!     oxdex-mini data/mini                 # interactive: one query per line

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use oxdex_index::InMemoryIndex;
use oxdex_query::{Bm25, search};

#[derive(Parser)]
#[command(version, about = "Search a folder of text files with BM25")]
struct Args {
    /// Folder of .txt files to index.
    dir: PathBuf,
    /// Query to run. Omit to read queries from stdin, one per line.
    query: Vec<String>,
    /// Number of results to show.
    #[arg(short, default_value_t = 10)]
    k: usize,
    /// BM25 k1 (term-frequency saturation).
    #[arg(long, default_value_t = 1.2)]
    k1: f64,
    /// BM25 b (length normalization).
    #[arg(long, default_value_t = 0.75)]
    b: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let bm25 = Bm25 { k1: args.k1, b: args.b };

    let started = Instant::now();
    let index = load_dir(&args.dir)?;
    eprintln!(
        "indexed {} docs, {} terms, avgdl {:.1} in {:.1} ms",
        index.num_docs(),
        index.num_terms(),
        index.avgdl(),
        started.elapsed().as_secs_f64() * 1e3
    );

    if !args.query.is_empty() {
        run_query(&index, &args.query.join(" "), args.k, &bm25);
        return Ok(());
    }

    let stdin = io::stdin();
    loop {
        eprint!("> ");
        io::stderr().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let q = line.trim();
        if !q.is_empty() {
            run_query(&index, q, args.k, &bm25);
        }
    }
    Ok(())
}

fn load_dir(dir: &Path) -> Result<InMemoryIndex> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    if paths.is_empty() {
        bail!("no .txt files in {}", dir.display());
    }
    // Sorted so doc IDs are stable from run to run.
    paths.sort();

    let mut index = InMemoryIndex::new();
    for path in paths {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let (title, body) = split_title(&raw, &path);
        index.add(&title, body);
    }
    Ok(index)
}

/// First non-empty line is the title; an empty file is titled by its file name.
fn split_title<'a>(raw: &'a str, path: &Path) -> (String, &'a str) {
    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        return (stem.into_owned(), "");
    }
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    (first.trim().to_owned(), rest)
}

fn run_query(index: &InMemoryIndex, query: &str, k: usize, bm25: &Bm25) {
    let started = Instant::now();
    let result = search(index, query, k, bm25);
    let took_ms = started.elapsed().as_secs_f64() * 1e3;

    println!("{} results in {:.3} ms", result.total_hits, took_ms);
    for (rank, hit) in result.hits.iter().enumerate() {
        let doc = index.doc(hit.doc);
        println!("{:>2}. {:>7.3}  {}", rank + 1, hit.score, doc.title);
        println!("             {}", preview(&doc.text, 90));
    }
}

fn preview(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}
