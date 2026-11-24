//! Agent-facing commands: memory (recall, remember, forget), code (symbol, repos), setup checks
//! (doctor), the bundled agent skill, and the Claude Code / Codex hooks.
//!
//! Tokens: memoryd takes `MUNINN_TOKEN`, else the output of `MUNINN_TOKEN_CMD` (run with sh -c,
//! e.g. a secret store lookup), else `~/.config/muninn/token`. indexd takes `MUNINN_INDEXD_TOKEN`,
//! else `~/.config/muninn/indexd-token`.
use serde_json::{Value, json};
use std::io::Read;

pub const SKILL: &str = include_str!("../../../skills/muninn/SKILL.md");

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// A setting from the environment, else from `~/.config/muninn/client.env` (KEY=VALUE lines,
/// written by deploy/client.sh), so hooks started by an agent see the same settings as a shell.
pub fn env(k: &str) -> Option<String> {
    static FILE: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    std::env::var(k)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            FILE.get_or_init(|| {
                std::fs::read_to_string(format!("{}/.config/muninn/client.env", home()))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim_start().starts_with('#'))
                    .filter_map(|l| l.split_once('='))
                    .map(|(a, b)| (a.trim().to_string(), b.trim().trim_matches('"').to_string()))
                    .collect()
            })
            .iter()
            .find(|(n, v)| n == k && !v.is_empty())
            .map(|(_, v)| v.clone())
        })
}

fn read_file(p: &str) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn memoryd_url() -> String {
    env("MUNINN_MEMORYD_URL").unwrap_or("http://127.0.0.1:7412".into())
}

pub fn token() -> Result<String, String> {
    if let Some(t) = env("MUNINN_TOKEN").or_else(|| env("MUNINN_MEMORYD_TOKEN")) {
        return Ok(t);
    }
    if let Some(cmd) = env("MUNINN_TOKEN_CMD") {
        let out = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .output()
            .map_err(|e| format!("MUNINN_TOKEN_CMD: {e}"))?;
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !t.is_empty() {
            return Ok(t);
        }
        return Err("MUNINN_TOKEN_CMD printed no token".into());
    }
    read_file(&format!("{}/.config/muninn/token", home())).ok_or_else(|| {
        "no token: set MUNINN_TOKEN, MUNINN_TOKEN_CMD or ~/.config/muninn/token".into()
    })
}

pub fn indexd_token() -> Option<String> {
    env("MUNINN_INDEXD_TOKEN")
        .or_else(|| read_file(&format!("{}/.config/muninn/indexd-token", home())))
}

fn agent(timeout_s: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(timeout_s)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn read(mut r: ureq::http::Response<ureq::Body>, what: &str) -> Result<Value, String> {
    let status = r.status().as_u16();
    let body = r
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("{what}: {e}"))?;
    if status >= 400 {
        return Err(format!(
            "{what}: HTTP {status}: {}",
            body.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("{what}: {e}"))
}

pub fn mem_post(route: &str, body: &Value, timeout_s: u64) -> Result<Value, String> {
    let mut req = agent(timeout_s)
        .post(&format!("{}/{route}", memoryd_url()))
        .header("Authorization", &format!("Bearer {}", token()?));
    if let Some(r) = env("MUNINN_RUN_ID") {
        req = req.header("X-Muninn-Run-Id", &r);
    }
    let r = req.send_json(body).map_err(|e| format!("{route}: {e}"))?;
    read(r, route)
}

pub fn index_get(path: &str) -> Result<Value, String> {
    let mut req = agent(30).get(&format!("{}{path}", crate::base_url()));
    if let Some(t) = indexd_token() {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    read(req.call().map_err(|e| format!("{path}: {e}"))?, path)
}

/// Flags of the form --name value; positional words are joined into one string.
struct Args {
    words: Vec<String>,
    flags: Vec<(String, String)>,
}

fn parse(a: &[String], valued: &[&str]) -> Result<Args, String> {
    let mut words = Vec::new();
    let mut flags = Vec::new();
    let mut it = a.iter();
    while let Some(x) = it.next() {
        if valued.contains(&x.as_str()) {
            let v = it.next().ok_or(format!("{x} needs a value"))?;
            flags.push((x.trim_start_matches('-').to_string(), v.clone()));
        } else if x.starts_with("--") {
            return Err(format!("unknown flag {x}"));
        } else {
            words.push(x.clone());
        }
    }
    Ok(Args { words, flags })
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
    fn text(&self) -> Option<String> {
        (!self.words.is_empty()).then(|| self.words.join(" "))
    }
    fn num(&self, k: &str) -> Result<Option<f64>, String> {
        self.get(k)
            .map(|v| {
                v.parse::<f64>()
                    .map_err(|_| format!("--{k} wants a number"))
            })
            .transpose()
    }
}

fn clip(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        one.chars().take(n - 1).collect::<String>() + "…"
    } else {
        one
    }
}

/// Runs an agent command. Returns the JSON value to print with --json, after printing the
/// human form itself when `json` is false.
pub fn run(cmd: &str, a: &[String], json_out: bool) -> Result<Value, String> {
    let v = match cmd {
        "recall" => {
            let p = parse(a, &["--scope", "--project", "-k"])?;
            let q = p
                .text()
                .ok_or("usage: muninn recall <query> [--scope S] [--project P] [-k N]")?;
            let mut b = json!({"query": q, "k": p.num("k")?.unwrap_or(8.0) as u64});
            if let Some(s) = p.get("scope") {
                b["scope"] = json!(s);
            }
            if let Some(s) = p.get("project") {
                b["project_id"] = json!(s);
            }
            let v = mem_post("recall", &b, 20)?;
            if !json_out {
                for h in v["hits"].as_array().into_iter().flatten() {
                    println!(
                        "{}  [{} {}] {:.2}  {}{}",
                        &h["id"].as_str().unwrap_or("")
                            [..8.min(h["id"].as_str().unwrap_or("").len())],
                        h["type"].as_str().unwrap_or(""),
                        h["scope"].as_str().unwrap_or(""),
                        h["score"].as_f64().unwrap_or(0.0),
                        clip(h["text"].as_str().unwrap_or(""), 200),
                        h["source"]
                            .as_str()
                            .map(|s| format!("  ({s})"))
                            .unwrap_or_default()
                    );
                }
            }
            v
        }
        "remember" => {
            let p = parse(
                a,
                &[
                    "--type",
                    "--scope",
                    "--project",
                    "--source",
                    "--confidence",
                    "--importance",
                    "--supersedes",
                ],
            )?;
            let text = p.text().ok_or("usage: muninn remember <text> [--type semantic|procedural|episodic] [--scope global|project|agent] [--project P] [--source S] [--importance X] [--confidence X] [--supersedes ID]")?;
            let mut b = json!({"text": text, "type": p.get("type").unwrap_or("semantic"), "scope": p.get("scope").unwrap_or("agent")});
            for (k, f) in [
                ("project_id", "project"),
                ("source", "source"),
                ("supersedes", "supersedes"),
            ] {
                if let Some(x) = p.get(f) {
                    b[k] = json!(x);
                }
            }
            for k in ["confidence", "importance"] {
                if let Some(x) = p.num(k)? {
                    b[k] = json!(x);
                }
            }
            let v = mem_post("remember", &b, 30)?;
            if !json_out {
                println!(
                    "{} {}",
                    v["status"]
                        .as_str()
                        .or(v["result"].as_str())
                        .unwrap_or("stored"),
                    v["id"].as_str().unwrap_or("")
                );
            }
            v
        }
        "forget" => {
            let id = a.first().ok_or("usage: muninn forget <id>")?;
            let v = mem_post("forget", &json!({"id": id}), 20)?;
            if !json_out {
                println!("forgot {id}");
            }
            v
        }
        "symbol" => {
            let p = parse(a, &["--repo", "-k"])?;
            let q = p
                .text()
                .ok_or("usage: muninn symbol <name> [--repo R] [-k N]")?;
            let mut path = format!(
                "/symbol?q={}&limit={}",
                urlencoding::encode(&q),
                p.num("k")?.unwrap_or(10.0) as u64
            );
            if let Some(r) = p.get("repo") {
                path += &format!("&repo={}", urlencoding::encode(r));
            }
            let v = index_get(&path)?;
            if !json_out {
                for h in v["hits"].as_array().into_iter().flatten() {
                    println!(
                        "{}:{}:{}-{}  {} {}",
                        h["repo"].as_str().unwrap_or(""),
                        h["path"].as_str().unwrap_or(""),
                        h["start_line"],
                        h["end_line"],
                        h["kind"].as_str().unwrap_or(""),
                        h["name"].as_str().unwrap_or("")
                    );
                }
            }
            v
        }
        "repos" => {
            let v = index_get("/repos")?;
            if !json_out {
                for r in v["repos"].as_array().into_iter().flatten() {
                    println!(
                        "{:<28} {:>7} chunks  {}",
                        r["repo"].as_str().or(r["name"].as_str()).unwrap_or(""),
                        r["chunks"].as_u64().unwrap_or(0),
                        r["branches"]
                            .as_array()
                            .map(|b| b
                                .iter()
                                .filter_map(|x| x["name"].as_str())
                                .collect::<Vec<_>>()
                                .join(","))
                            .unwrap_or_default()
                    );
                }
            }
            v
        }
        "doctor" => doctor(json_out),
        "skill" => {
            // --install DIR writes DIR/muninn/SKILL.md (e.g. ~/.claude/skills); without it, print it.
            if let Some(dir) = parse(a, &["--install"])?.get("install") {
                let d = std::path::Path::new(dir).join("muninn");
                std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
                let p = d.join("SKILL.md");
                std::fs::write(&p, SKILL).map_err(|e| format!("{}: {e}", p.display()))?;
                if !json_out {
                    println!("installed {}", p.display());
                }
                return Ok(json!({"installed": p.display().to_string()}));
            }
            if !json_out {
                print!("{SKILL}");
            }
            json!({"name": "muninn", "skill": SKILL})
        }
        "mcp-header" => {
            // Claude Code headersHelper (JSON object) or, with --plain, the bare header value.
            let t = token()?;
            if a.iter().any(|x| x == "--plain") {
                println!("Bearer {t}");
            } else {
                println!("{}", json!({"Authorization": format!("Bearer {t}")}));
            }
            Value::Null
        }
        "hook" => {
            hook(a.first().map(String::as_str).unwrap_or(""));
            Value::Null
        }
        _ => return Err(format!("unknown command {cmd}")),
    };
    Ok(v)
}

fn doctor(json_out: bool) -> Value {
    let mut checks = Vec::new();
    let mut add = |name: &str, ok: bool, detail: String| {
        if !json_out {
            println!("{} {name}: {detail}", if ok { "ok  " } else { "FAIL" });
        }
        checks.push(json!({"check": name, "ok": ok, "detail": detail}));
    };
    let probe = |url: String| match agent(5).get(&url).call() {
        Ok(r) if r.status().is_success() => (true, url),
        Ok(r) => (false, format!("{url}: HTTP {}", r.status())),
        Err(e) => (false, format!("{url}: {e}")),
    };
    let (ok, d) = probe(format!("{}/healthz", crate::base_url()));
    add("indexd", ok, d);
    let (ok, d) = probe(format!("{}/healthz", memoryd_url()));
    add("memoryd", ok, d);
    match indexd_token() {
        Some(_) => match index_get("/repos") {
            Ok(v) => add(
                "index token",
                true,
                format!(
                    "{} repos indexed",
                    v["repos"].as_array().map_or(0, |r| r.len())
                ),
            ),
            Err(e) => add("index token", false, e),
        },
        None => add(
            "index token",
            false,
            "set MUNINN_INDEXD_TOKEN or ~/.config/muninn/indexd-token".into(),
        ),
    }
    match token().and_then(|_| mem_post("recall", &json!({"query": "muninn doctor", "k": 1}), 15)) {
        Ok(_) => add("memory token", true, "recall works".into()),
        Err(e) => add("memory token", false, e),
    }
    let ok = checks.iter().all(|c| c["ok"] == json!(true));
    json!({"ok": ok, "checks": checks})
}

// ---------------------------------------------------------------- hooks

/// Repos the pre hook guards: `MUNINN_HOOK_REPOS`, comma-separated paths; `~` is expanded and a
/// trailing `*` matches any directory name with that prefix (`~/Projects/acme-*`).
fn guarded() -> Vec<(String, bool)> {
    env("MUNINN_HOOK_REPOS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|p| {
            let p = p
                .strip_prefix("~/")
                .map(|r| format!("{}/{r}", home()))
                .unwrap_or(p.into());
            match p.strip_suffix('*') {
                Some(pre) => (pre.to_string(), true),
                None => (p.trim_end_matches('/').to_string(), false),
            }
        })
        .collect()
}

/// True when `s` mentions a guarded path (as a path, not just a prefix of a longer name).
pub fn mentions_guarded(s: &str, g: &[(String, bool)]) -> bool {
    g.iter().any(|(p, star)| {
        s.match_indices(p.as_str()).any(|(i, _)| {
            *star
                || matches!(
                    s[i + p.len()..].chars().next(),
                    None | Some('/' | ' ' | '\'' | '"' | '\t' | '\n')
                )
        })
    })
}

/// True when a shell command searches files with grep, rg, ag, ack, find or git grep. Only the
/// first command of each pipeline counts (`cargo test | grep FAILED` filters output, it does not
/// search the repo), plus `xargs <tool>` anywhere in a pipeline.
pub fn is_search(cmd: &str) -> bool {
    const TOOLS: [&str; 8] = ["grep", "rg", "ag", "ack", "find", "egrep", "fgrep", "ugrep"];
    let base = |t: &str| t.rsplit('/').next().unwrap_or(t).to_string();
    cmd.split(|c: char| ";&()\n".contains(c)).any(|stmt| {
        stmt.split('|').enumerate().any(|(i, stage)| {
            let toks: Vec<String> = stage
                .split_whitespace()
                .skip_while(|t| {
                    t.contains('=') || matches!(*t, "sudo" | "time" | "command" | "nice" | "env")
                })
                .map(base)
                .collect();
            let Some(first) = toks.first() else {
                return false;
            };
            if first == "xargs" {
                return toks[1..].iter().any(|t| TOOLS.contains(&t.as_str()));
            }
            i == 0
                && (TOOLS.contains(&first.as_str())
                    || (first == "git" && toks.get(1).is_some_and(|t| t == "grep")))
        })
    })
}

/// The pre hook's decision for one tool call: deny a search in a guarded repo.
pub fn should_deny(tool: &str, inp: &Value, cwd: &str, g: &[(String, bool)]) -> bool {
    let cwd_in = mentions_guarded(&format!("{cwd}/"), g);
    match tool {
        "Grep" | "Glob" => {
            let p = inp["path"].as_str().unwrap_or(cwd);
            let abs = if p.starts_with('/') {
                p.to_string()
            } else {
                format!("{cwd}/{p}")
            };
            mentions_guarded(&format!("{abs}/"), g)
        }
        "Bash" | "shell" | "local_shell" | "exec_command" | "shell_command" => {
            let cmd = match &inp["command"] {
                Value::Array(a) => a
                    .iter()
                    .map(|x| x.as_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(" "),
                v => v.as_str().unwrap_or("").to_string(),
            };
            if !is_search(&cmd) {
                return false;
            }
            if mentions_guarded(&cmd, g) {
                return true;
            }
            // In a guarded checkout, deny unless the search points at an absolute path elsewhere.
            cwd_in
                && !cmd
                    .split_whitespace()
                    .any(|w| w.trim_matches(|c| c == '\'' || c == '"').starts_with('/'))
        }
        _ => false,
    }
}

/// MCP server names the hooks treat as Muninn (`MUNINN_MCP_NAME`, comma-separated; the first one
/// is named in messages). More than one helps while clients move to a new server name.
fn mcp_names() -> Vec<String> {
    let v: Vec<String> = env("MUNINN_MCP_NAME")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if v.is_empty() {
        vec!["muninn".into()]
    } else {
        v
    }
}

fn marker(session: &str) -> String {
    let s: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    format!(
        "{}/.local/state/muninn-hook/{}",
        home(),
        if s.is_empty() { "unknown".into() } else { s }
    )
}

fn emit(event: &str, fields: Value) {
    let mut o = json!({"hookEventName": event});
    if let (Some(o), Some(f)) = (o.as_object_mut(), fields.as_object()) {
        o.extend(f.clone());
    }
    println!("{}", json!({"hookSpecificOutput": o}));
}

/// `muninn hook prompt|pre|post` with the hook event on stdin. Fails open: errors go to stderr
/// and the tool call or prompt goes through.
fn hook(kind: &str) {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    let ev: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    if env("MUNINN_HOOK_OFF").is_some() {
        return;
    }
    let session = ev["session_id"].as_str().unwrap_or("");
    let names = mcp_names();
    let mcp = &names[0];
    match kind {
        "pre" => {
            if std::path::Path::new(&marker(session)).exists() {
                return;
            }
            let cwd = ev["cwd"].as_str().map(String::from).unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            });
            if should_deny(
                ev["tool_name"].as_str().unwrap_or(""),
                &ev["tool_input"],
                &cwd,
                &guarded(),
            ) {
                emit(
                    "PreToolUse",
                    json!({"permissionDecision": "deny", "permissionDecisionReason": format!(
                    "This repo is indexed by Muninn. Query the index first: mcp__{mcp}__symbol_lookup for identifiers, \
                     mcp__{mcp}__code_search for concepts or exact strings (mode lexical), and mcp__{mcp}__recall for \
                     past decisions. After any {mcp} tool call in this session, grep and Glob are allowed again.")}),
                );
            }
        }
        "post" => {
            let tool = ev["tool_name"].as_str().unwrap_or("");
            if names
                .iter()
                .any(|n| tool.starts_with(&format!("mcp__{n}__")))
            {
                let m = marker(session);
                if let Some(d) = std::path::Path::new(&m).parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(m, "");
            }
        }
        "prompt" => {
            let text = ev["prompt"].as_str().unwrap_or("").trim();
            if text.chars().count() < 20 || text.starts_with('/') {
                return;
            }
            let q: String = text.chars().take(500).collect();
            let min = env("MUNINN_HOOK_MIN_SCORE")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.3);
            match mem_post("recall", &json!({"query": q, "k": 5}), 8) {
                Ok(v) => {
                    let lines: Vec<String> = v["hits"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|h| h["score"].as_f64().unwrap_or(0.0) >= min)
                        .map(|h| {
                            format!(
                                "- [{}] {} (src: {}, id {})",
                                h["type"].as_str().unwrap_or(""),
                                h["text"].as_str().unwrap_or(""),
                                h["source"].as_str().unwrap_or("?"),
                                h["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .chars()
                                    .take(8)
                                    .collect::<String>()
                            )
                        })
                        .collect();
                    if !lines.is_empty() {
                        emit(
                            "UserPromptSubmit",
                            json!({"additionalContext": format!(
                            "muninn recall for this prompt (shared memory; verify against code before acting, and \
                             `remember` new durable facts at the end):\n{}", lines.join("\n"))}),
                        );
                    }
                }
                Err(e) => eprintln!("muninn hook: {e}"),
            }
        }
        _ => eprintln!("usage: muninn hook prompt|pre|post  (hook event JSON on stdin)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> Vec<(String, bool)> {
        vec![
            ("/home/u/Projects/acme-".into(), true),
            ("/home/u/Projects/muninn".into(), false),
        ]
    }

    #[test]
    fn bundled_skill_follows_the_agent_skills_spec() {
        let fm = SKILL
            .strip_prefix("---\n")
            .and_then(|s| s.split_once("\n---\n"))
            .map(|(f, _)| f)
            .expect("frontmatter");
        let field = |k: &str| {
            fm.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}: ")))
                .unwrap_or("")
        };
        assert_eq!(field("name"), "muninn");
        let d = field("description");
        assert!(!d.is_empty() && d.chars().count() <= 1024);
        for l in fm.lines().filter(|l| !l.starts_with(' ')) {
            let key = l.split(':').next().unwrap();
            assert!(
                [
                    "name",
                    "description",
                    "license",
                    "compatibility",
                    "metadata",
                    "allowed-tools"
                ]
                .contains(&key),
                "frontmatter key {key}"
            );
        }
        assert!(SKILL.lines().count() < 500);
    }

    #[test]
    fn detects_search_commands() {
        assert!(is_search("rg -n foo src"));
        assert!(is_search("cd x && git grep -n foo"));
        assert!(is_search("/usr/bin/grep -r b src"));
        assert!(!is_search("cat a | /usr/bin/grep b"));
        assert!(!is_search("cargo build --release"));
        assert!(!is_search("echo grepping"));
        assert!(!is_search("cargo test 2>&1 | grep 'test result'"));
        assert!(is_search("git ls-files | xargs rg -n foo"));
        assert!(is_search("cd src && LC_ALL=C grep -rn foo ."));
    }

    #[test]
    fn matches_guarded_paths_only_as_paths() {
        assert!(mentions_guarded("rg x /home/u/Projects/muninn/src", &g()));
        assert!(mentions_guarded("rg x /home/u/Projects/acme-web", &g()));
        assert!(!mentions_guarded("rg x /home/u/Projects/muninn-old", &g()));
    }

    #[test]
    fn denies_searches_in_guarded_repos() {
        let cwd = "/home/u/Projects/muninn";
        assert!(should_deny(
            "Bash",
            &json!({"command": "rg foo"}),
            cwd,
            &g()
        ));
        assert!(!should_deny(
            "Bash",
            &json!({"command": "rg foo /etc"}),
            cwd,
            &g()
        ));
        assert!(!should_deny(
            "Bash",
            &json!({"command": "cargo test"}),
            cwd,
            &g()
        ));
        assert!(should_deny("Grep", &json!({"pattern": "x"}), cwd, &g()));
        assert!(!should_deny("Grep", &json!({"pattern": "x"}), "/tmp", &g()));
        assert!(should_deny(
            "Glob",
            &json!({"path": "/home/u/Projects/acme-api"}),
            "/tmp",
            &g()
        ));
        assert!(!should_deny(
            "Bash",
            &json!({"command": "rg foo"}),
            "/tmp",
            &[]
        ));
    }
}
