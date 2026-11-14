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
    pub bm25: Bm25,
    /// Multiplier for title-field matches.
    pub title_boost: f64,
    /// An AND query with fewer hits than this is rerun as OR.
    pub min_and_hits: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            bm25: Bm25::default(),
            title_boost: 2.0,
            min_and_hits: 10,
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
    /// Documents that matched (before paging).
    pub total_hits: usize,
    pub mode: MatchMode,
}

struct TermLists<C> {
    body: Option<C>,
    title: Option<C>,
    idf_body: f64,
    idf_title: f64,
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
}

impl<I: SearchIndex> Scorer<'_, I> {
    fn score<C: PostingCursor>(&self, doc: DocId, lists: &[TermLists<C>]) -> f64 {
        let bm = &self.params.bm25;
        let body_len = self.fields.body.doc_len(doc);
        let title_len = self.fields.title.map_or(0, |t| t.doc_len(doc));
        let mut score = 0.0;
        for l in lists {
            if let Some(c) = &l.body
                && c.doc() == doc
            {
                score += bm.term_score(l.idf_body, c.tf(), body_len, self.avgdl_body);
            }
            if let Some(c) = &l.title
                && c.doc() == doc
            {
                score += self.params.title_boost * bm.term_score(l.idf_title, c.tf(), title_len, self.avgdl_title);
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
    };

    let mut lists: Vec<TermLists<I::Cursor<'_>>> = Vec::with_capacity(query.terms.len());
    for term in &query.terms {
        let body = fields.body.cursor(term);
        let title = fields.title.and_then(|t| t.cursor(term));
        if body.is_none() && title.is_none() {
            if mode == Mode::All {
                // A required term that is nowhere in the index: no AND matches.
                return FieldResult { hits: Vec::new(), total_hits: 0, mode: label };
            }
            continue;
        }
        lists.push(TermLists {
            idf_body: body.as_ref().map_or(0.0, |c| Bm25::idf(n, c.df())),
            idf_title: title.as_ref().map_or(0.0, |c| Bm25::idf(n, c.df())),
            body,
            title,
        });
    }
    let mut excluded: Vec<I::Cursor<'_>> = query.excluded.iter().filter_map(|t| fields.body.cursor(t)).collect();

    let mut top = TopK::new(offset.saturating_add(k));
    let mut total = 0usize;

    if lists.is_empty() {
        return FieldResult { hits: Vec::new(), total_hits: 0, mode: label };
    }

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

    let hits = top.into_sorted_vec().into_iter().skip(offset).collect();
    FieldResult { hits, total_hits: total, mode: label }
}
