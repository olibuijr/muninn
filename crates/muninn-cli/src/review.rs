//! `muninn review`: work the memory review queue of muninn-memoryd (`POST /review`,
//! `POST /review/decide`). The queue is per owner, so the token decides whose items are listed;
//! deciding needs the `memory:forget` scope (the board token).
use serde_json::{Value, json};

pub const USAGE: &str = "usage:
  muninn review list [--json]
  muninn review approve <id> [--supersede]
  muninn review reject <id>
  muninn review approve-all --older-than <N>d|<N>h|<N>w [--supersede] [--dry-run]";

fn post(path: &str, body: &Value) -> Result<Value, String> {
    let token = crate::agent::token()?;
    ureq::post(&format!("{}/{path}", crate::agent::memoryd_url()))
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(body)
        .map_err(|e| format!("{path}: {e}"))?
        .into_body()
        .read_json()
        .map_err(|e| format!("{path}: {e}"))
}

/// `7d`, `12h`, `2w` (or a bare number of days) -> days.
pub fn parse_age(s: &str) -> Option<f64> {
    let (n, mult) = match s.chars().last()? {
        'd' => (&s[..s.len() - 1], 1.0),
        'h' => (&s[..s.len() - 1], 1.0 / 24.0),
        'w' => (&s[..s.len() - 1], 7.0),
        _ => (s, 1.0),
    };
    n.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v * mult)
}

/// Ids of queue items at least `days` old. Err when the server does not report ages (memoryd
/// older than the `age_days` field), so nothing is approved blindly.
pub fn older_than(items: &[Value], days: f64) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for it in items {
        let age = it["age_days"]
            .as_f64()
            .ok_or("memoryd does not report age_days; deploy a newer muninn-memoryd")?;
        if age >= days
            && let Some(id) = it["id"].as_str()
        {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        one
    } else {
        one.chars().take(n - 1).collect::<String>() + "…"
    }
}

fn list_items() -> Result<Vec<Value>, String> {
    Ok(post("review", &json!({}))?["review"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn decide(id: &str, approve: bool, supersede: bool) -> Result<Value, String> {
    post(
        "review/decide",
        &json!({"id": id, "approve": approve, "supersede": supersede}),
    )
}

pub fn run(a: &[String]) -> Result<(), String> {
    let flag = |f: &str| a.iter().any(|x| x == f);
    match a.first().map(String::as_str) {
        Some("list") => {
            let items = list_items()?;
            if flag("--json") {
                println!("{}", Value::Array(items));
                return Ok(());
            }
            for it in &items {
                let age = it["age_days"]
                    .as_f64()
                    .map_or("?".into(), |d| format!("{d:.0}d"));
                let conflict = it["conflicts_with"]
                    .as_str()
                    .map_or(String::new(), |c| format!(" conflicts_with={c}"));
                println!(
                    "{}  {:>4}  {}/{}  conf={:.2}{}\n    {}",
                    it["id"].as_str().unwrap_or("?"),
                    age,
                    it["type"].as_str().unwrap_or("?"),
                    it["scope"].as_str().unwrap_or("?"),
                    it["confidence"].as_f64().unwrap_or(0.0),
                    conflict,
                    clip(it["text"].as_str().unwrap_or(""), 160)
                );
            }
            eprintln!("{} item(s) in review", items.len());
            Ok(())
        }
        Some(cmd @ ("approve" | "reject")) => {
            let id = a.get(1).filter(|x| !x.starts_with('-')).ok_or(USAGE)?;
            let r = decide(id, cmd == "approve", flag("--supersede"))?;
            println!("{r}");
            Ok(())
        }
        Some("approve-all") => {
            let days = a
                .iter()
                .position(|x| x == "--older-than")
                .and_then(|i| a.get(i + 1))
                .and_then(|s| parse_age(s))
                .ok_or("approve-all requires --older-than <N>d")?;
            let ids = older_than(&list_items()?, days)?;
            let dry = flag("--dry-run");
            let mut failed = 0;
            for id in &ids {
                if dry {
                    println!("would approve {id}");
                    continue;
                }
                match decide(id, true, flag("--supersede")) {
                    Ok(r) => println!("{r}"),
                    Err(e) => {
                        eprintln!("{id}: {e}");
                        failed += 1;
                    }
                }
            }
            eprintln!(
                "{} {} item(s) older than {days} day(s){}",
                if dry { "would approve" } else { "approved" },
                ids.len() - failed,
                if failed > 0 {
                    format!(", {failed} failed")
                } else {
                    String::new()
                }
            );
            if failed > 0 {
                return Err(format!("{failed} decision(s) failed"));
            }
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_parse_in_days() {
        assert_eq!(parse_age("7d"), Some(7.0));
        assert_eq!(parse_age("2w"), Some(14.0));
        assert_eq!(parse_age("12h"), Some(0.5));
        assert_eq!(parse_age("3"), Some(3.0));
        assert_eq!(parse_age("d"), None);
        assert_eq!(parse_age("-1d"), None);
        assert_eq!(parse_age("x"), None);
        assert_eq!(parse_age(""), None);
    }

    #[test]
    fn older_than_selects_by_age_and_refuses_without_ages() {
        let items = vec![
            json!({"id": "a", "age_days": 10.0}),
            json!({"id": "b", "age_days": 7.0}),
            json!({"id": "c", "age_days": 1.5}),
        ];
        assert_eq!(older_than(&items, 7.0).unwrap(), ["a", "b"]);
        assert!(older_than(&items, 30.0).unwrap().is_empty());
        assert!(older_than(&[json!({"id": "x"})], 7.0).is_err());
    }

    #[test]
    fn clip_flattens_and_truncates() {
        assert_eq!(clip("a\n  b", 10), "a b");
        assert_eq!(clip("abcdef", 4), "abc…");
    }
}
