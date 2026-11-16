//! Field-aware matching and scoring.
//!
//! Each query term gets a [`TermLists`]: its posting cursor in the body field
//! (title + article text) and in the title field (title + redirect titles).
//! A document matches a term if either field contains it, so a search for
//! "usa" can find "United States" through its redirect even if the word never
//! appears in the article text.
//!
//! - AND: leapfrog intersection. Start from the shortest list; every other list
//!   `advance`s (skip pointers) to the candidate, and any overshoot becomes the
//!   lead's next target.
//! - OR: document-at-a-time union, always taking the smallest current doc ID.
//! - `-term`: a candidate is dropped if the term's body list contains it.
//! - If AND finds fewer than `min_and_hits`, the search reruns as OR.
//!
//! Score = BM25(body) + `title_boost` × BM25(title), each field with its own
//! document lengths and average length.

use oxdex_index::{DocId, NO_MORE_DOCS, PostingCursor, SearchIndex};

use crate::parse::{Mode, ParsedQuery};
use crate::{Bm25, Hit, TopK};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// BM25 for the article body.
    pub bm25: Bm25,
    /// BM25 for the title field (title plus redirect titles).
    pub title_bm25: Bm25,
    /// Multiplier for title-field matches.
    pub title_boost: f64,
    /// Added to docs in `ParsedQuery::exact_title_docs`. Large enough that an
    /// exact title match outranks any BM25 score.
    pub exact_title_bonus: f64,
    /// An AND query with fewer hits than this is rerun as OR.
    pub min_and_hits: usize,
    /// Use WAND for OR queries. Off = score every matching doc (the
    /// reference the tests compare against).
    pub wand: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            // Tuned on benches/eval-simple.tsv (M5). Less length normalization
            // than the textbook b = 0.75: the articles people look for are the
            // long, central ones, which full normalization pushes down.
            bm25: Bm25 { k1: 1.2, b: 0.3 },
            // The title field also holds every redirect, so a popular article
            // has a long title field; normalizing it would punish popularity.
            title_bm25: Bm25 { k1: 1.2, b: 0.0 },
            title_boost: 2.0,
            exact_title_bonus: 1000.0,
            min_and_hits: 10,
            wand: true,
        }
    }
}

/// The indexes one search runs over. `title` is optional.
pub struct Fields<'a, I> {
    pub body: &'a I,
    pub title: Option<&'a I>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    And,
    Or,
    /// AND found too few results, so OR was used.
    OrFallback,
}

impl MatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchMode::And => "and",
            MatchMode::Or => "or",
            MatchMode::OrFallback => "and_fell_back_to_or",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldResult {
    /// Best hits first: at most `k` of them, after skipping `offset`.
    pub hits: Vec<Hit>,
    /// Documents that matched (before paging). A lower bound when
    /// `total_exact` is false.
    pub total_hits: usize,
    /// False when WAND skipped documents without counting them.
    pub total_exact: bool,
    pub mode: MatchMode,
}

struct TermLists<C> {
    body: Option<C>,
    title: Option<C>,
    idf_body: f64,
    idf_title: f64,
    /// Highest score this term can add to any document (WAND bound).
    ub: f64,
}

impl<C: PostingCursor> TermLists<C> {
    #[inline]
    fn doc(&self) -> DocId {
        let b = self.body.as_ref().map_or(NO_MORE_DOCS, |c| c.doc());
        let t = self.title.as_ref().map_or(NO_MORE_DOCS, |c| c.doc());
        b.min(t)
    }

    fn advance(&mut self, target: DocId) {
        if let Some(c) = self.body.as_mut() {
            c.advance(target);
        }
        if let Some(c) = self.title.as_mut() {
            c.advance(target);
        }
    }

    /// Moves past the current doc in whichever fields sit on it.
    fn next(&mut self) {
        let d = self.doc();
        for c in [self.body.as_mut(), self.title.as_mut()].into_iter().flatten() {
            if c.doc() == d {
                c.next();
            }
        }
    }

    fn cost(&self) -> u64 {
        let b = self.body.as_ref().map_or(0, |c| u64::from(c.df()));
        let t = self.title.as_ref().map_or(0, |c| u64::from(c.df()));
        b + t
    }
}

struct Scorer<'a, I> {
    fields: &'a Fields<'a, I>,
    params: &'a Params,
    avgdl_body: f64,
    avgdl_title: f64,
    exact: &'a [DocId],
}

impl<I: SearchIndex> Scorer<'_, I> {
    fn score<C: PostingCursor>(&self, doc: DocId, lists: &[TermLists<C>]) -> f64 {
        let bm = &self.params.bm25;
        let body_len = self.fields.body.doc_len(doc);
        let title_len = self.fields.title.map_or(0, |t| t.doc_len(doc));
        let mut score = if self.exact.contains(&doc) { self.params.exact_title_bonus } else { 0.0 };
        for l in lists {
            if let Some(c) = &l.body
                && c.doc() == doc
            {
                score += bm.term_score(l.idf_body, c.tf(), body_len, self.avgdl_body);
            }
            if let Some(c) = &l.title
                && c.doc() == doc
            {
                score += self.params.title_boost * self.params.title_bm25.term_score(l.idf_title, c.tf(), title_len, self.avgdl_title);
            }
        }
        score
    }
}

fn is_excluded<C: PostingCursor>(excluded: &mut [C], doc: DocId) -> bool {
    excluded.iter_mut().any(|c| {
        c.advance(doc);
        c.doc() == doc
    })
}

/// Runs a parsed query and returns hits `offset..offset + k`.
pub fn search_fields<I: SearchIndex>(
    fields: &Fields<'_, I>,
    query: &ParsedQuery,
    k: usize,
    offset: usize,
    params: &Params,
) -> FieldResult {
    match query.mode {
        Mode::Any => run(fields, query, k, offset, params, Mode::Any, MatchMode::Or),
        Mode::All => {
            let and = run(fields, query, k, offset, params, Mode::All, MatchMode::And);
            if and.total_hits >= params.min_and_hits || query.terms.len() <= 1 {
                and
            } else {
                run(fields, query, k, offset, params, Mode::Any, MatchMode::OrFallback)
            }
        }
    }
}

fn run<I: SearchIndex>(
    fields: &Fields<'_, I>,
    query: &ParsedQuery,
    k: usize,
    offset: usize,
    params: &Params,
    mode: Mode,
    label: MatchMode,
) -> FieldResult {
    let n = fields.body.num_docs();
    let scorer = Scorer {
        fields,
        params,
        avgdl_body: fields.body.avgdl(),
        avgdl_title: fields.title.map_or(0.0, |t| t.avgdl()),
        exact: &query.exact_title_docs,
    };

    let mut lists: Vec<TermLists<I::Cursor<'_>>> = Vec::with_capacity(query.terms.len());
    for term in &query.terms {
        let body = fields.body.cursor(term);
        let title = fields.title.and_then(|t| t.cursor(term));
        if body.is_none() && title.is_none() {
            if mode == Mode::All {
                // A required term that is nowhere in the index: no AND matches.
                return FieldResult { hits: Vec::new(), total_hits: 0, total_exact: true, mode: label };
            }
            continue;
        }
        let idf_body = body.as_ref().map_or(0.0, |c| Bm25::idf(n, c.df()));
        let idf_title = title.as_ref().map_or(0.0, |c| Bm25::idf(n, c.df()));
        // BM25 rises with tf and falls with length, so the list's largest tf
        // and shortest doc bound every posting's score.
        let ub = body.as_ref().map_or(0.0, |c| {
            params.bm25.term_score(idf_body, c.max_tf(), c.min_doc_len(), scorer.avgdl_body)
        }) + title.as_ref().map_or(0.0, |c| {
            params.title_boost * params.title_bm25.term_score(idf_title, c.max_tf(), c.min_doc_len(), scorer.avgdl_title)
        });
        lists.push(TermLists { idf_body, idf_title, ub, body, title });
    }
    let mut excluded: Vec<I::Cursor<'_>> = query.excluded.iter().filter_map(|t| fields.body.cursor(t)).collect();

    let mut top = TopK::new(offset.saturating_add(k));
    let mut total = 0usize;

    if lists.is_empty() {
        return FieldResult { hits: Vec::new(), total_hits: 0, total_exact: true, mode: label };
    }
    let mut total_exact = true;

    if mode == Mode::All {
        // Cheapest list leads. Scores are summed in query order, so keep a
        // separate leapfrog order instead of reordering `lists`.
        let mut order: Vec<usize> = (0..lists.len()).collect();
        order.sort_by_key(|&i| lists[i].cost());
        let lead = order[0];
        'outer: loop {
            let target = lists[lead].doc();
            if target == NO_MORE_DOCS {
                break;
            }
            for &i in &order[1..] {
                lists[i].advance(target);
                let d = lists[i].doc();
                if d == NO_MORE_DOCS {
                    break 'outer;
                }
                if d != target {
                    lists[lead].advance(d);
                    continue 'outer;
                }
            }
            if !is_excluded(&mut excluded, target) {
                total += 1;
                top.push(Hit { doc: target, score: scorer.score(target, &lists) });
            }
            lists[lead].next();
        }
    } else if params.wand {
        // Exact-title docs carry a bonus no bound accounts for, so score them
        // up front with fresh cursors and skip them in the main loop.
        let mut exact: Vec<DocId> = query.exact_title_docs.clone();
        exact.sort_unstable();
        exact.dedup();
        for &d in &exact {
            let mut fresh: Vec<TermLists<I::Cursor<'_>>> = query
                .terms
                .iter()
                .filter_map(|t| {
                    let body = fields.body.cursor(t);
                    let title = fields.title.and_then(|f| f.cursor(t));
                    (body.is_some() || title.is_some())
                        .then(|| TermLists { body, title, idf_body: 0.0, idf_title: 0.0, ub: 0.0 })
                })
                .collect();
            // Same idfs as the main lists, in the same order (both skip absent terms).
            for (f, l) in fresh.iter_mut().zip(&lists) {
                f.idf_body = l.idf_body;
                f.idf_title = l.idf_title;
                f.advance(d);
            }
            let mut ex: Vec<I::Cursor<'_>> = query.excluded.iter().filter_map(|t| fields.body.cursor(t)).collect();
            if fresh.iter().any(|f| f.doc() == d) && !is_excluded(&mut ex, d) {
                total += 1;
                top.push(Hit { doc: d, score: scorer.score(d, &fresh) });
            }
        }

        let mut order: Vec<usize> = (0..lists.len()).collect();
        loop {
            order.sort_by_key(|&i| lists[i].doc());
            // Pivot: the first list (in doc order) at which the summed bounds
            // could reach the current k-th score.
            let threshold = top.threshold();
            let mut acc = 0.0;
            let mut pivot = None;
            for (p, &i) in order.iter().enumerate() {
                if lists[i].doc() == NO_MORE_DOCS {
                    break;
                }
                acc += lists[i].ub;
                // >= (not >): a later doc can tie the k-th score. The epsilon
                // covers float summation order between bound and score.
                if threshold.is_none_or(|t| acc * (1.0 + 1e-9) >= t) {
                    pivot = Some(p);
                    break;
                }
            }
            let Some(p) = pivot else {
                if lists.iter().any(|l| l.doc() != NO_MORE_DOCS) {
                    total_exact = false;
                }
                break;
            };
            let pivot_doc = lists[order[p]].doc();
            if lists[order[0]].doc() == pivot_doc {
                // Every list before the pivot sits on pivot_doc: score it.
                if exact.binary_search(&pivot_doc).is_err() && !is_excluded(&mut excluded, pivot_doc) {
                    total += 1;
                    top.push(Hit { doc: pivot_doc, score: scorer.score(pivot_doc, &lists) });
                }
                for l in lists.iter_mut() {
                    if l.doc() == pivot_doc {
                        l.next();
                    }
                }
            } else {
                // Docs before pivot_doc can't make the top k: skip them.
                total_exact = false;
                for &i in &order[..p] {
                    lists[i].advance(pivot_doc);
                }
            }
        }
    } else {
        loop {
            let doc = lists.iter().map(TermLists::doc).min().unwrap_or(NO_MORE_DOCS);
            if doc == NO_MORE_DOCS {
                break;
            }
            if !is_excluded(&mut excluded, doc) {
                total += 1;
                top.push(Hit { doc, score: scorer.score(doc, &lists) });
            }
            for l in lists.iter_mut() {
                if l.doc() == doc {
                    l.next();
                }
            }
        }
    }

    if !total_exact && query.excluded.is_empty() {
        // Pruned docs went uncounted. The union is at least as long as the
        // longest single list (with exclusions there is no such bound).
        let longest = lists
            .iter()
            .flat_map(|l| [l.body.as_ref().map(|c| c.df()), l.title.as_ref().map(|c| c.df())])
            .flatten()
            .max()
            .unwrap_or(0);
        total = total.max(longest as usize);
    }
    let hits = top.into_sorted_vec().into_iter().skip(offset).collect();
    FieldResult { hits, total_hits: total, total_exact, mode: label }
}
