use serde_json::Value;

mod agent;
mod eval;
mod review;

const USAGE: &str = "usage: muninn <command> [--json]
  muninn recall <query> [--scope global|project|agent] [--project P] [-k N]
  muninn remember <text> [--type semantic|procedural|episodic] [--scope S] [--project P]
                  [--source S] [--importance X] [--confidence X] [--supersedes ID]
  muninn forget <id>
  muninn symbol <name> [--repo R] [-k N]
  muninn repos
  muninn doctor                 check the daemons and tokens
  muninn skill [--install DIR]  print the agent skill, or write DIR/muninn/SKILL.md
  muninn hook prompt|pre|post   Claude Code / Codex hooks (event JSON on stdin)
  muninn mcp-header [--plain]   Authorization header for MCP clients (headersHelper)
  muninn --version
  muninn search <query> [--repo R] [--branch B] [--mode lexical|semantic|hybrid] [-k N]
  muninn eval <cases.tsv>... [--mode lexical|semantic|hybrid|all] [-k N] [--repo R] [--branch B]
              [--json out.json] [--min-recall X] [--expand none|heuristic|llm|both]
              [--param key=value]... [-v]
  muninn eval --compare a.json b.json [-v]
  muninn expand <query> [--mode heuristic|llm|both] [-n N]
  muninn review list|approve <id>|reject <id>|approve-all --older-than 7d  (memory review queue)
env: MUNINN_INDEXD_URL (default http://127.0.0.1:7410), MUNINN_INDEXD_TOKEN or ~/.config/muninn/indexd-token;
     MUNINN_MEMORYD_URL (default http://127.0.0.1:7412), MUNINN_TOKEN, MUNINN_TOKEN_CMD or ~/.config/muninn/token;
     settings also come from ~/.config/muninn/client.env (KEY=VALUE);
     hooks: MUNINN_HOOK_REPOS (comma-separated paths, trailing * allowed), MUNINN_MCP_NAME (muninn);
     expand llm: MUNINN_CHAT_URL or MUNINN_EMBED_URL, MUNINN_CHAT_MODEL, MUNINN_LLAMA_API_KEY,
     MUNINN_EXPAND_TIMEOUT_MS";

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2)
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let json_out = a.iter().any(|x| x == "--json")
        && a.first().map(String::as_str) != Some("review")
        && a.first().map(String::as_str) != Some("eval");
    if json_out {
        a.retain(|x| x != "--json");
    }
    if let Some(
        cmd @ ("recall" | "remember" | "forget" | "symbol" | "repos" | "doctor" | "skill" | "hook"
        | "mcp-header"),
    ) = a.first().map(String::as_str)
    {
        match agent::run(cmd, &a[1..], json_out) {
            Ok(v) => {
                if json_out && !v.is_null() {
                    println!("{v}");
                }
                if cmd == "doctor" && v["ok"] != serde_json::json!(true) {
                    std::process::exit(1);
                }
            }
            Err(e) => {
                if json_out {
                    println!("{}", serde_json::json!({"error": e}));
                } else {
                    eprintln!("muninn: {e}");
                }
                std::process::exit(1);
            }
        }
        return;
    }
    match (a.first().map(String::as_str), a.len()) {
        (Some("--version" | "-V"), _) => println!("muninn {}", env!("CARGO_PKG_VERSION")),
        (Some("search"), n) if n >= 2 => search(&a[1..]),
        (Some("eval"), n) if n >= 2 => std::process::exit(eval::run(&a[1..])),
        (Some("review"), _) => {
            if let Err(e) = review::run(&a[1..]) {
                die(&e);
            }
        }
        (Some("expand"), n) if n >= 2 => expand(&a[1..]),
        _ => die(USAGE),
    }
}

pub fn base_url() -> String {
    agent::env("MUNINN_INDEXD_URL").unwrap_or("http://127.0.0.1:7410".into())
}

/// GET with the bearer token from `MUNINN_INDEXD_TOKEN` when set.
pub fn get_json(url: &str) -> Result<Value, String> {
    let mut req = ureq::get(url);
    if let Ok(token) = std::env::var("MUNINN_INDEXD_TOKEN") {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    req.call()
        .map_err(|e| e.to_string())?
        .into_body()
        .read_json()
        .map_err(|e| e.to_string())
}

fn search(a: &[String]) {
    let mut url = format!("{}/search?q={}", base_url(), urlencoding::encode(&a[0]));
    for w in a[1..].chunks(2) {
        let key = match w[0].as_str() {
            "--repo" => "repo",
            "--branch" => "branch",
            "--mode" => "mode",
            "-k" => "k",
            o => die(&format!("unknown flag {o}")),
        };
        url += &format!(
            "&{key}={}",
            urlencoding::encode(w.get(1).map(String::as_str).unwrap_or(""))
        );
    }
    let v = get_json(&url).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1)
    });
    for h in v["hits"].as_array().into_iter().flatten() {
        println!(
            "{}:{}:{}-{}  {} {}  ({:.4})",
            h["repo"].as_str().unwrap_or(""),
            h["path"].as_str().unwrap_or(""),
            h["start_line"],
            h["end_line"],
            h["kind"].as_str().unwrap_or(""),
            h["symbol"].as_str().unwrap_or("-"),
            h["score"].as_f64().unwrap_or(0.0)
        );
    }
}

/// `muninn expand`: print an expansion, the derived lexical query and dense text, and timing.
/// `-n N` repeats the call N times with a fresh expander (no cache) and prints p50/max.
fn expand(a: &[String]) {
    use muninn_core::expand::{self, ExpandMode};
    let mut mode = ExpandMode::Llm;
    let mut n = 1usize;
    let mut q = None;
    let mut it = a.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mode" => {
                mode = it
                    .next()
                    .map(|m| m.parse())
                    .unwrap_or(Err("--mode needs a value".into()))
                    .unwrap_or_else(|e: String| die(&e))
            }
            "-n" => {
                n = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| die("-n wants a positive number"))
            }
            f if f.starts_with('-') && f.len() > 1 => die(&format!("unknown flag {f}")),
            f => q = Some(f.to_string()),
        }
    }
    let q = q.unwrap_or_else(|| die(USAGE));
    let mut ms = Vec::new();
    let mut e = expand::Expansion::default();
    for _ in 0..n {
        let ex = expand::expander(mode);
        let t = std::time::Instant::now();
        e = ex.expand(&q);
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!("identifiers: {}", e.identifiers.join(" "));
    println!("keywords:    {}", e.keywords.join(" "));
    println!("hyde:        {}", e.hyde.as_deref().unwrap_or("-"));
    println!("lexical:     {}", expand::lexical_query(&q, &e));
    println!(
        "dense:       {}",
        expand::dense_text(&q, &e).replace('\n', " / ")
    );
    ms.sort_by(f64::total_cmp);
    println!(
        "time:        p50 {:.1} ms, max {:.1} ms over {n} call(s){}",
        ms[(ms.len() - 1) / 2],
        ms[ms.len() - 1],
        if e.is_empty() && mode != ExpandMode::Heuristic {
            "  (empty: timeout or unparsable reply)"
        } else {
            ""
        }
    );
}
