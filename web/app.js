// Oxdex search page. State lives in the URL (?q=&offset=) so results are
// linkable and the back button works.
"use strict";

const PAGE = 10;
const $ = (id) => document.getElementById(id);
const form = $("search-form");
const input = $("q");
const statusEl = $("status");
const list = $("results");
const pager = $("pager");

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function setStatus(text, isError = false) {
  statusEl.textContent = text;
  statusEl.classList.toggle("error", isError);
}

function readUrl() {
  const p = new URLSearchParams(location.search);
  return { q: p.get("q") || "", offset: Math.max(0, parseInt(p.get("offset") || "0", 10) || 0) };
}

function go(q, offset, push = true) {
  const url = `/?q=${encodeURIComponent(q)}${offset ? `&offset=${offset}` : ""}`;
  if (push) history.pushState(null, "", url);
  run(q, offset);
}

let inflight = null;

async function run(q, offset) {
  input.value = q;
  if (!q.trim()) {
    list.innerHTML = "";
    pager.hidden = true;
    setStatus("");
    document.title = "Oxdex";
    return;
  }
  document.title = `${q} – Oxdex`;
  if (inflight) inflight.abort();
  inflight = new AbortController();
  const started = performance.now();
  let res, body;
  try {
    res = await fetch(`/search?q=${encodeURIComponent(q)}&k=${PAGE}&offset=${offset}`, { signal: inflight.signal });
    body = await res.json();
  } catch (e) {
    if (e.name !== "AbortError") setStatus(`Request failed: ${e.message}`, true);
    return;
  }
  if (!res.ok) {
    list.innerHTML = "";
    pager.hidden = true;
    setStatus(body.error || `HTTP ${res.status}`, true);
    return;
  }
  const rtt = performance.now() - started;
  const n = (body.total_exact ? "" : "at least ") + body.total_hits.toLocaleString();
  const fell = body.mode === "and_fell_back_to_or" ? " · few documents had every word, so showing any-word matches" : "";
  setStatus(`${n} result${body.total_hits === 1 ? "" : "s"} in ${body.took_ms.toFixed(2)} ms (round trip ${rtt.toFixed(0)} ms)${fell}`);

  list.innerHTML = body.results
    .map(
      (r) => `<li>
        <a class="title" href="${escapeHtml(r.url)}" target="_blank" rel="noopener">${escapeHtml(r.title)}</a>
        <div class="meta">score ${r.score.toFixed(3)} · doc ${r.doc_id} · <a href="/doc/${r.doc_id}">text</a></div>
        <p class="snippet">${r.snippet}</p>
      </li>`,
    )
    .join("");
  list.start = offset + 1;

  const pages = Math.ceil(body.total_hits / PAGE);
  pager.hidden = pages <= 1;
  $("prev").disabled = offset === 0;
  $("next").disabled = offset + PAGE >= body.total_hits || offset + PAGE > 10000;
  $("page").textContent = `Page ${Math.floor(offset / PAGE) + 1} of ${pages.toLocaleString()}`;
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  go(input.value.trim(), 0);
});
$("prev").addEventListener("click", () => {
  const { q, offset } = readUrl();
  go(q, Math.max(0, offset - PAGE));
  scrollTo(0, 0);
});
$("next").addEventListener("click", () => {
  const { q, offset } = readUrl();
  go(q, offset + PAGE);
  scrollTo(0, 0);
});
window.addEventListener("popstate", () => {
  const { q, offset } = readUrl();
  run(q, offset);
});

fetch("/stats")
  .then((r) => (r.ok ? r.json() : null))
  .then((s) => {
    if (s) $("footer").textContent = `${s.num_docs.toLocaleString()} articles · ${s.num_terms.toLocaleString()} terms · index ${(s.index_bytes / 1048576).toFixed(0)} MB`;
  })
  .catch(() => {});

const initial = readUrl();
run(initial.q, initial.offset);
