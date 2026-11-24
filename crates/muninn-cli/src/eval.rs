//! `muninn eval`: retrieval-quality harness against a running indexd.
//!
//! Case files are TSV, `#` starts a comment line:
//! `query<TAB>repo<TAB>expected[<TAB>tag]`, where `expected` is `path` or `path#symbol`, with
//! alternatives separated by `|`. A hit matches when its path equals an alternative's path and,
//! for `path#symbol`, its symbol equals too. `tag` (optional) groups cases in the report.
//!
//! `--expand heuristic|llm|both` rewrites each query client-side with `muninn_core::expand`
//! (sends `lexical_query` as `q`, so the expanded text reaches BM25, the embedder and the
//! reranker alike); `--param k=v` passes extra `/search` parameters; `--compare a.json b.json`
//! diffs two saved runs.
use muninn_core::expand::{ExpandMode, lexical_query};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

const MODES: [&str; 3] = ["lexical", "semantic", "hybrid"];

#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub path: String,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub query: String,
    pub repo: String,
    pub expected: Vec<Expect>,
    pub tag: String,
}

pub fn parse_expected(s: &str) -> Vec<Expect> {
    s.split('|')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| match a.split_once('#') {
            Some((p, sym)) if !sym.is_empty() => Expect {
                path: p.into(),
                symbol: Some(sym.into()),
            },
            Some((p, _)) => Expect {
                path: p.into(),
                symbol: None,
            },
            None => Expect {
                path: a.into(),
                symbol: None,
            },
        })
        .collect()
}

pub fn parse_cases(text: &str, file: &str) -> Result<Vec<Case>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 || f[0].trim().is_empty() {
            return Err(format!(
                "{file}:{}: want query<TAB>repo<TAB>expected[<TAB>tag]",
                i + 1
            ));
        }
        let expected = parse_expected(f[2]);
        if expected.is_empty() {
            return Err(format!("{file}:{}: empty expected", i + 1));
        }
        out.push(Case {
            query: f[0].trim().into(),
            repo: f[1].trim().into(),
            expected,
            tag: f.get(3).map(|t| t.trim()).unwrap_or("").into(),
        });
    }
    Ok(out)
}

pub fn hit_matches(hit: &Value, exp: &[Expect], path_only: bool) -> bool {
    let path = hit["path"].as_str().unwrap_or("");
    let sym = hit["symbol"].as_str();
    exp.iter()
        .any(|e| e.path == path && (path_only || e.symbol.is_none() || e.symbol.as_deref() == sym))
}

/// 1-based rank of the first matching hit.
pub fn rank(hits: &[Value], exp: &[Expect], path_only: bool) -> Option<usize> {
    hits.iter()
        .position(|h| hit_matches(h, exp, path_only))
        .map(|i| i + 1)
}

/// Nearest-rank percentile of an unsorted sample.
pub fn percentile(xs: &[f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let idx = ((p / 100.0 * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
    v[idx]
}

#[derive(Debug, Clone)]
struct Outcome {
    rank: Option<usize>,
    path_rank: Option<usize>,
    ms: f64,
    error: Option<String>,
    top: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub recall: f64,
    pub mrr: f64,
    pub path_recall: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub errors: usize,
}

fn stats(rows: &[&Outcome], k: usize) -> Stats {
    let n = rows.len();
    if n == 0 {
        return Stats::default();
    }
    let within = |r: Option<usize>| r.is_some_and(|r| r <= k);
    let ms: Vec<f64> = rows.iter().map(|o| o.ms).collect();
    Stats {
        n,
        recall: rows.iter().filter(|o| within(o.rank)).count() as f64 / n as f64,
        mrr: rows
            .iter()
            .map(|o| match o.rank {
                Some(r) if r <= k => 1.0 / r as f64,
                _ => 0.0,
            })
            .sum::<f64>()
            / n as f64,
        path_recall: rows.iter().filter(|o| within(o.path_rank)).count() as f64 / n as f64,
        p50_ms: percentile(&ms, 50.0),
        p95_ms: percentile(&ms, 95.0),
        errors: rows.iter().filter(|o| o.error.is_some()).count(),
    }
}

fn stats_json(s: &Stats) -> Value {
    let r = |x: f64| (x * 10000.0).round() / 10000.0;
    json!({
        "n": s.n, "recall": r(s.recall), "mrr": r(s.mrr), "path_recall": r(s.path_recall),
        "p50_ms": r(s.p50_ms), "p95_ms": r(s.p95_ms), "errors": s.errors,
    })
}

struct Opts {
    files: Vec<String>,
    modes: Vec<String>,
    k: usize,
    repo: Option<String>,
    branch: Option<String>,
    json: Option<String>,
    min_recall: Option<f64>,
    verbose: bool,
    expand: ExpandMode,
    /// Extra `/search` query parameters, sent verbatim (e.g. `expand=heuristic`).
    params: Vec<(String, String)>,
    /// `--compare a.json b.json`: diff two saved runs instead of querying.
    compare: Option<(String, String)>,
}

fn parse_opts(a: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        files: vec![],
        modes: vec!["hybrid".into()],
        k: 10,
        repo: None,
        branch: None,
        json: None,
        min_recall: None,
        verbose: false,
        expand: ExpandMode::None,
        params: vec![],
        compare: None,
    };
    let mut it = a.iter();
    while let Some(arg) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--mode" => {
                let m = val("--mode")?;
                o.modes = match m.as_str() {
                    "all" => MODES.iter().map(|s| s.to_string()).collect(),
                    m if MODES.contains(&m) => vec![m.into()],
                    m => return Err(format!("unknown mode {m}")),
                }
            }
            "-k" => o.k = val("-k")?.parse().map_err(|_| "-k wants a number")?,
            "--repo" => o.repo = Some(val("--repo")?),
            "--branch" => o.branch = Some(val("--branch")?),
            "--json" => o.json = Some(val("--json")?),
            "--min-recall" => {
                o.min_recall = Some(
                    val("--min-recall")?
                        .parse()
                        .map_err(|_| "--min-recall wants a number")?,
                )
            }
            "--expand" => o.expand = val("--expand")?.parse()?,
            "--param" => {
                let p = val("--param")?;
                let (k, v) = p.split_once('=').ok_or("--param wants key=value")?;
                if k.is_empty() {
                    return Err("--param wants key=value".into());
                }
                o.params.push((k.into(), v.into()));
            }
            "--compare" => {
                let a = val("--compare")?;
                let b = it.next().cloned().ok_or("--compare needs two files")?;
                o.compare = Some((a, b));
            }
            "-v" | "--verbose" => o.verbose = true,
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown flag {f}")),
            f => o.files.push(f.into()),
        }
    }
    if o.files.is_empty() && o.compare.is_none() {
        return Err("no case files".into());
    }
    if o.k == 0 {
        return Err("-k must be > 0".into());
    }
    Ok(o)
}

/// The `/search` URL for one case: `q` is the (possibly expanded) query text.
fn search_url(base: &str, q: &str, repo: &str, mode: &str, o: &Opts) -> String {
    let mut url = format!(
        "{base}/search?q={}&mode={mode}&k={}",
        urlencoding::encode(q),
        o.k
    );
    if !repo.is_empty() {
        url += &format!("&repo={}", urlencoding::encode(repo));
    }
    if let Some(b) = &o.branch {
        url += &format!("&branch={}", urlencoding::encode(b));
    }
    for (k, v) in &o.params {
        url += &format!("&{}={}", urlencoding::encode(k), urlencoding::encode(v));
    }
    url
}

fn query(c: &Case, q: &str, mode: &str, o: &Opts) -> Outcome {
    let url = search_url(&crate::base_url(), q, &c.repo, mode, o);
    let t = Instant::now();
    let res = crate::get_json(&url);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let (hits, error) = match res {
        Ok(v) => {
            let hits = v["hits"].as_array().cloned().unwrap_or_default();
            // An empty answer almost always means the repo is not loaded (restart, re-ingest).
            let err = v["error"].as_str().map(String::from).or_else(|| {
                hits.is_empty()
                    .then(|| format!("no hits (busy_repos {})", v["busy_repos"]))
            });
            (hits, err)
        }
        Err(e) => (vec![], Some(e)),
    };
    Outcome {
        rank: rank(&hits, &c.expected, false),
        path_rank: rank(&hits, &c.expected, true),
        ms,
        error,
        top: hits
            .iter()
            .take(3)
            .map(|h| {
                format!(
                    "{}#{}",
                    h["path"].as_str().unwrap_or(""),
                    h["symbol"].as_str().unwrap_or("")
                )
            })
            .collect(),
    }
}

fn print_row(label: &str, s: &Stats) {
    println!(
        "  {label:<28} {:>5} {:>9.3} {:>7.3} {:>8.3} {:>8.1} {:>8.1} {:>4}",
        s.n, s.recall, s.mrr, s.path_recall, s.p50_ms, s.p95_ms, s.errors
    );
}

fn header(title: &str, k: usize) {
    println!(
        "  {:<28} {:>5} {:>9} {:>7} {:>8} {:>8} {:>8} {:>4}",
        title,
        "n",
        format!("recall@{k}"),
        "MRR",
        format!("path@{k}"),
        "p50 ms",
        "p95 ms",
        "err"
    );
}

/// Group outcomes by `key` (cases with an empty key are skipped), print one table and return
/// the JSON object of the groups.
fn table(
    title: &str,
    cases: &[Case],
    rows: &[Outcome],
    k: usize,
    key: impl Fn(&Case) -> Option<String>,
) -> Value {
    let mut groups: BTreeMap<String, Vec<&Outcome>> = BTreeMap::new();
    for (c, r) in cases.iter().zip(rows) {
        if let Some(g) = key(c) {
            groups.entry(g).or_default().push(r);
        }
    }
    let mut out = json!({});
    if groups.is_empty() {
        return out;
    }
    header(title, k);
    for (g, rs) in &groups {
        let s = stats(rs, k);
        print_row(g, &s);
        out[g] = stats_json(&s);
    }
    out
}

/// Expansion applied client-side to every case before it is sent.
struct Sent {
    q: String,
    expand_ms: f64,
}

fn expand_cases(cases: &[Case], mode: ExpandMode) -> Vec<Sent> {
    let ex = muninn_core::expand::expander(mode);
    cases
        .iter()
        .map(|c| {
            let t = Instant::now();
            let e = ex.expand(&c.query);
            Sent {
                expand_ms: t.elapsed().as_secs_f64() * 1000.0,
                q: lexical_query(&c.query, &e),
            }
        })
        .collect()
}

fn expand_name(m: ExpandMode) -> &'static str {
    match m {
        ExpandMode::None => "none",
        ExpandMode::Heuristic => "heuristic",
        ExpandMode::Llm => "llm",
        ExpandMode::Both => "both",
    }
}

/// Returns the process exit code.
pub fn run(a: &[String]) -> i32 {
    let o = match parse_opts(a) {
        Ok(o) => o,
        Err(e) => crate::die(&format!("{e}\n{}", crate::USAGE)),
    };
    if let Some((a, b)) = &o.compare {
        return compare(a, b, o.verbose);
    }
    let mut cases = Vec::new();
    for f in &o.files {
        let text = std::fs::read_to_string(f).unwrap_or_else(|e| crate::die(&format!("{f}: {e}")));
        match parse_cases(&text, f) {
            Ok(c) => cases.extend(c),
            Err(e) => crate::die(&e),
        }
    }
    if let Some(r) = &o.repo {
        cases.retain(|c| &c.repo == r);
    }
    if cases.is_empty() {
        crate::die("no cases to run");
    }

    let sent = expand_cases(&cases, o.expand);
    let exp_ms: Vec<f64> = sent.iter().map(|s| s.expand_ms).collect();
    if o.expand != ExpandMode::None {
        println!(
            "expand {}: p50 {:.1} ms, p95 {:.1} ms per query (client-side, before /search)",
            expand_name(o.expand),
            percentile(&exp_ms, 50.0),
            percentile(&exp_ms, 95.0)
        );
    }

    // outcomes[mode][case]
    let mut outcomes: Vec<Vec<Outcome>> = Vec::new();
    for m in &o.modes {
        outcomes.push(
            cases
                .iter()
                .zip(&sent)
                .map(|(c, s)| query(c, &s.q, m, &o))
                .collect(),
        );
    }

    let mut summary = json!({});
    let mut overall: Vec<(String, Stats)> = Vec::new();
    for (mi, m) in o.modes.iter().enumerate() {
        let rows = &outcomes[mi];
        println!(
            "\nmode {m} (k={}, {} cases, expand {})",
            o.k,
            cases.len(),
            expand_name(o.expand)
        );
        let by_repo = table("repo", &cases, rows, o.k, |c| Some(c.repo.clone()));
        let by_tag = table("tag", &cases, rows, o.k, |c| {
            (!c.tag.is_empty()).then(|| c.tag.clone())
        });
        let by_repo_tag = table("repo / tag", &cases, rows, o.k, |c| {
            (!c.tag.is_empty()).then(|| format!("{} / {}", c.repo, c.tag))
        });
        let all: Vec<&Outcome> = rows.iter().collect();
        let st = stats(&all, o.k);
        print_row("overall", &st);
        if o.verbose {
            println!("  misses:");
            for ((c, r), s) in cases.iter().zip(rows).zip(&sent) {
                if !r.rank.is_some_and(|x| x <= o.k) {
                    println!(
                        "    [{}] {:?}{} -> want {} got {}{}",
                        c.repo,
                        c.query,
                        if s.q != c.query {
                            format!(" (sent {:?})", s.q)
                        } else {
                            String::new()
                        },
                        fmt_expected(&c.expected),
                        r.top.join(", "),
                        r.error
                            .as_deref()
                            .map(|e| format!(" ERROR {e}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        summary[m] = json!({
            "overall": stats_json(&st), "by_repo": by_repo, "by_tag": by_tag,
            "by_repo_tag": by_repo_tag,
        });
        overall.push((m.clone(), st));
    }

    if o.modes.len() > 1 {
        println!("\nsummary (k={}, expand {})", o.k, expand_name(o.expand));
        header("mode", o.k);
        for (m, s) in &overall {
            print_row(m, s);
        }
    }

    if let Some(path) = &o.json {
        let case_rows: Vec<Value> = cases
            .iter()
            .zip(&sent)
            .enumerate()
            .map(|(ci, (c, s))| {
                let mut res = json!({});
                for (mi, m) in o.modes.iter().enumerate() {
                    let r = &outcomes[mi][ci];
                    res[m] = json!({
                        "rank": r.rank, "path_rank": r.path_rank,
                        "ms": (r.ms * 10.0).round() / 10.0, "error": r.error, "top": r.top,
                    });
                }
                let mut row = json!({
                    "query": c.query, "repo": c.repo, "expected": fmt_expected(&c.expected),
                    "tag": c.tag, "results": res,
                });
                if o.expand != ExpandMode::None {
                    row["sent"] = json!(s.q);
                    row["expand_ms"] = json!((s.expand_ms * 10.0).round() / 10.0);
                }
                row
            })
            .collect();
        let params: Value = o
            .params
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let doc = json!({
            "k": o.k, "branch": o.branch, "indexd": crate::base_url(),
            "expand": expand_name(o.expand), "params": params,
            "expand_ms": {"p50": percentile(&exp_ms, 50.0), "p95": percentile(&exp_ms, 95.0)},
            "files": o.files, "modes": summary, "cases": case_rows,
        });
        let text = serde_json::to_string_pretty(&doc).unwrap() + "\n";
        if let Err(e) = std::fs::write(path, text) {
            eprintln!("{path}: {e}");
            return 1;
        }
    }

    if let Some(min) = o.min_recall {
        let failed: Vec<String> = overall
            .iter()
            .filter(|(_, s)| s.recall < min)
            .map(|(m, s)| format!("{m} {:.3}", s.recall))
            .collect();
        if !failed.is_empty() {
            eprintln!("recall@{} below {min}: {}", o.k, failed.join(", "));
            return 1;
        }
    }
    0
}

/// One delta row: `a`, `b` and `b - a` for recall and MRR.
#[derive(Debug, PartialEq)]
pub struct Delta {
    pub group: String,
    pub n: u64,
    pub recall: (f64, f64),
    pub mrr: (f64, f64),
}

/// Rows for every group present in both runs' summaries of one mode (overall first).
pub fn deltas(a: &Value, b: &Value) -> Vec<Delta> {
    let mut out = Vec::new();
    let mut push = |group: String, x: &Value, y: &Value| {
        if x.is_object() && y.is_object() {
            out.push(Delta {
                group,
                n: y["n"].as_u64().unwrap_or(0),
                recall: (
                    x["recall"].as_f64().unwrap_or(0.0),
                    y["recall"].as_f64().unwrap_or(0.0),
                ),
                mrr: (
                    x["mrr"].as_f64().unwrap_or(0.0),
                    y["mrr"].as_f64().unwrap_or(0.0),
                ),
            });
        }
    };
    push("overall".into(), &a["overall"], &b["overall"]);
    for (sect, label) in [("by_repo", "repo"), ("by_tag", "tag"), ("by_repo_tag", "")] {
        if let Some(m) = a[sect].as_object() {
            for (g, x) in m {
                let name = if label.is_empty() {
                    g.clone()
                } else {
                    format!("{label} {g}")
                };
                push(name, x, &b[sect][g]);
            }
        }
    }
    out
}

fn load_json(p: &str) -> Value {
    let t = std::fs::read_to_string(p).unwrap_or_else(|e| crate::die(&format!("{p}: {e}")));
    serde_json::from_str(&t).unwrap_or_else(|e| crate::die(&format!("{p}: {e}")))
}

/// `--compare a.json b.json`: recall/MRR per mode, repo and tag, b relative to a. With `-v`,
/// also the cases that entered or left the top k.
fn compare(pa: &str, pb: &str, verbose: bool) -> i32 {
    let (a, b) = (load_json(pa), load_json(pb));
    let k = b["k"].as_u64().unwrap_or(0);
    if a["k"] != b["k"] {
        eprintln!("warning: k differs ({} vs {})", a["k"], b["k"]);
    }
    println!(
        "a = {pa} (expand {})\nb = {pb} (expand {})",
        a["expand"].as_str().unwrap_or("none"),
        b["expand"].as_str().unwrap_or("none")
    );
    let Some(modes) = b["modes"].as_object() else {
        crate::die(&format!("{pb}: no modes"))
    };
    for (m, bm) in modes {
        let am = &a["modes"][m];
        if !am.is_object() {
            continue;
        }
        println!("\nmode {m} (k={k})");
        println!(
            "  {:<32} {:>4} {:>7} {:>7} {:>7}   {:>6} {:>6} {:>7}",
            "group", "n", "rec a", "rec b", "Δrec", "MRR a", "MRR b", "ΔMRR"
        );
        for d in deltas(am, bm) {
            println!(
                "  {:<32} {:>4} {:>7.3} {:>7.3} {:>+7.3}   {:>6.3} {:>6.3} {:>+7.3}",
                d.group,
                d.n,
                d.recall.0,
                d.recall.1,
                d.recall.1 - d.recall.0,
                d.mrr.0,
                d.mrr.1,
                d.mrr.1 - d.mrr.0
            );
        }
        if verbose {
            let key = |c: &Value| format!("{}\t{}", c["repo"], c["query"]);
            let ranks: BTreeMap<String, Option<u64>> = a["cases"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| (key(c), c["results"][m]["rank"].as_u64()))
                .collect();
            let within = |r: Option<u64>| r.is_some_and(|r| r <= k);
            for c in b["cases"].as_array().into_iter().flatten() {
                let Some(ra) = ranks.get(&key(c)) else {
                    continue;
                };
                let rb = c["results"][m]["rank"].as_u64();
                if within(*ra) != within(rb) {
                    println!(
                        "  {} [{}] {}  rank {:?} -> {:?}",
                        if within(rb) { "+" } else { "-" },
                        c["repo"].as_str().unwrap_or(""),
                        c["query"].as_str().unwrap_or(""),
                        ra,
                        rb
                    );
                }
            }
        }
    }
    0
}

fn fmt_expected(e: &[Expect]) -> String {
    e.iter()
        .map(|x| match &x.symbol {
            Some(s) => format!("{}#{s}", x.path),
            None => x.path.clone(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cases_and_alternatives() {
        let t = "# comment\n\nparse http\trepo-a\tsrc/a.rs#parse|src/b.rs\tname\nq2\tr\tx.rs\n";
        let c = parse_cases(t, "t.tsv").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].tag, "name");
        assert_eq!(
            c[0].expected,
            vec![
                Expect {
                    path: "src/a.rs".into(),
                    symbol: Some("parse".into())
                },
                Expect {
                    path: "src/b.rs".into(),
                    symbol: None
                }
            ]
        );
        assert_eq!(c[1].tag, "");
        assert!(parse_cases("only\ttwo\n", "t").is_err());
    }

    #[test]
    fn matches_path_and_symbol() {
        let exp = parse_expected("src/a.rs#parse|src/b.rs");
        let hits = vec![
            json!({"path": "src/a.rs", "symbol": "other"}),
            json!({"path": "src/a.rs", "symbol": null}),
            json!({"path": "src/b.rs", "symbol": "x"}),
        ];
        assert_eq!(rank(&hits, &exp, false), Some(3));
        assert_eq!(rank(&hits, &exp, true), Some(1));
        assert_eq!(rank(&hits[..2], &exp, false), None);
        let hit = json!({"path": "src/a.rs", "symbol": "parse"});
        assert!(hit_matches(&hit, &exp, false));
    }

    #[test]
    fn percentiles_and_stats() {
        let xs: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&xs, 50.0), 50.0);
        assert_eq!(percentile(&xs, 95.0), 95.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
        let mk = |rank, ms| Outcome {
            rank,
            path_rank: rank,
            ms,
            error: None,
            top: vec![],
        };
        let rows = [
            mk(Some(1), 10.0),
            mk(Some(4), 20.0),
            mk(None, 30.0),
            mk(Some(12), 40.0),
        ];
        let refs: Vec<&Outcome> = rows.iter().collect();
        let s = stats(&refs, 10);
        assert_eq!(s.n, 4);
        assert!((s.recall - 0.5).abs() < 1e-9);
        assert!((s.mrr - (1.0 + 0.25) / 4.0).abs() < 1e-9);
    }

    #[test]
    fn parses_flags() {
        let a: Vec<String> = ["c.tsv", "--mode", "all", "-k", "5", "--min-recall", "0.5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_opts(&a).unwrap();
        assert_eq!(o.modes.len(), 3);
        assert_eq!(o.k, 5);
        assert_eq!(o.min_recall, Some(0.5));
        assert!(parse_opts(&["--mode".into()]).is_err());
        assert!(parse_opts(&["c.tsv".into(), "--bogus".into()]).is_err());
    }

    #[test]
    fn parses_expand_param_and_compare() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = parse_opts(&s(&[
            "c.tsv",
            "--expand",
            "heuristic",
            "--param",
            "expand=llm",
            "--param",
            "x=a=b",
        ]))
        .unwrap();
        assert_eq!(o.expand, ExpandMode::Heuristic);
        assert_eq!(
            o.params,
            vec![("expand".into(), "llm".into()), ("x".into(), "a=b".into())]
        );
        assert!(parse_opts(&s(&["c.tsv", "--expand", "bogus"])).is_err());
        assert!(parse_opts(&s(&["c.tsv", "--param", "novalue"])).is_err());
        let o = parse_opts(&s(&["--compare", "a.json", "b.json"])).unwrap();
        assert_eq!(o.compare, Some(("a.json".into(), "b.json".into())));
        assert!(parse_opts(&s(&["--compare", "a.json"])).is_err());
        let o = parse_opts(&s(&["c.tsv", "--repo", "r", "--param", "expand=heuristic"])).unwrap();
        let url = search_url("http://h", "a b&c", "r", "hybrid", &o);
        assert_eq!(
            url,
            "http://h/search?q=a%20b%26c&mode=hybrid&k=10&repo=r&expand=heuristic"
        );
    }

    #[test]
    fn deltas_cover_overall_repo_and_tag() {
        let a = json!({"overall": {"n": 2, "recall": 0.5, "mrr": 0.25},
            "by_repo": {"r": {"n": 2, "recall": 0.5, "mrr": 0.25}},
            "by_tag": {"real": {"n": 2, "recall": 0.5, "mrr": 0.25}, "gone": {"n": 1}},
            "by_repo_tag": {}});
        let b = json!({"overall": {"n": 2, "recall": 1.0, "mrr": 0.75},
            "by_repo": {"r": {"n": 2, "recall": 1.0, "mrr": 0.75}},
            "by_tag": {"real": {"n": 2, "recall": 1.0, "mrr": 0.75}},
            "by_repo_tag": {}});
        let d = deltas(&a, &b);
        let names: Vec<&str> = d.iter().map(|x| x.group.as_str()).collect();
        assert_eq!(names, vec!["overall", "repo r", "tag real"]);
        assert_eq!(d[0].recall, (0.5, 1.0));
        assert_eq!(d[2].mrr, (0.25, 0.75));
    }
}
