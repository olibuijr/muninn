//! SCIP (https://github.com/sourcegraph/scip) reader: decodes the subset we need and derives
//! definition, reference and call-edge rows.
use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Index {
    #[prost(message, repeated, tag = "2")]
    pub documents: Vec<Document>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Document {
    #[prost(string, tag = "1")]
    pub relative_path: String,
    #[prost(message, repeated, tag = "2")]
    pub occurrences: Vec<Occurrence>,
    #[prost(message, repeated, tag = "3")]
    pub symbols: Vec<SymbolInfo>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Occurrence {
    #[prost(int32, repeated, tag = "1")]
    pub range: Vec<i32>,
    #[prost(string, tag = "2")]
    pub symbol: String,
    #[prost(int32, tag = "3")]
    pub symbol_roles: i32,
    #[prost(int32, repeated, tag = "7")]
    pub enclosing_range: Vec<i32>,
}
#[derive(Clone, PartialEq, Message)]
pub struct SymbolInfo {
    #[prost(string, tag = "1")]
    pub symbol: String,
    #[prost(string, repeated, tag = "3")]
    pub documentation: Vec<String>,
    #[prost(int32, tag = "5")]
    pub kind: i32,
    #[prost(string, tag = "6")]
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i32,
    pub end_line: i32,
    pub doc: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    pub symbol: String,
    pub path: String,
    pub line: i32,
    pub col: i32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub caller: String,
    pub callee: String,
    pub path: String,
    pub line: i32,
}
#[derive(Debug, Default)]
pub struct Graph {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    pub calls: Vec<Call>,
}

pub fn kind_name(k: i32) -> &'static str {
    match k {
        17 | 12 => "function",
        26 | 66 | 68 => "method",
        9 => "constructor",
        49 | 60 => "struct",
        11 | 36 => "enum",
        54 | 53 | 21 => "interface",
        55 => "trait",
        2 | 29 | 35 => "module",
        8 => "field",
        15 | 61 => "variable",
        _ => "symbol",
    }
}

/// Last human-readable descriptor of a SCIP symbol string, e.g. `foo().` -> `foo`.
pub fn symbol_name(sym: &str) -> String {
    let body = sym.splitn(5, ' ').nth(4).unwrap_or(sym);
    let mut name = String::new();
    let mut cur = String::new();
    let mut tick = false;
    for c in body.chars() {
        match c {
            '`' => tick = !tick,
            '/' | '#' | '.' | '(' | ')' | ':' | '[' | ']' | '!' if !tick => {
                if !cur.is_empty() {
                    name = std::mem::take(&mut cur);
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        name = cur;
    }
    name
}

fn is_local(s: &str) -> bool {
    s.is_empty() || s.starts_with("local ")
}

fn callable(sym: &str, kind: &str) -> bool {
    matches!(kind, "function" | "method" | "constructor") || sym.ends_with("().")
}

pub fn decode(bytes: &[u8]) -> Result<Index, prost::DecodeError> {
    Index::decode(bytes)
}

/// Build graph rows from an index; `prefix` is prepended to every document path.
pub fn graph(ix: &Index, prefix: &str) -> Graph {
    let mut g = Graph::default();
    let mut kinds = std::collections::HashMap::new();
    for d in &ix.documents {
        for s in &d.symbols {
            kinds.insert(s.symbol.clone(), kind_name(s.kind));
        }
    }
    for d in &ix.documents {
        let path = format!("{prefix}{}", d.relative_path);
        let info: std::collections::HashMap<&str, &SymbolInfo> =
            d.symbols.iter().map(|s| (s.symbol.as_str(), s)).collect();
        let mut local_defs: Vec<Def> = Vec::new();
        for o in &d.occurrences {
            if is_local(&o.symbol) || o.range.len() < 3 {
                continue;
            }
            if o.symbol_roles & 1 == 0 {
                continue;
            }
            let si = info.get(o.symbol.as_str());
            let kind = kinds
                .get(&o.symbol)
                .copied()
                .unwrap_or("symbol")
                .to_string();
            let (start, end) = match o.enclosing_range.as_slice() {
                [a, _, _] => (*a, *a),
                [a, _, b, _] => (*a, *b),
                _ => (o.range[0], o.range[0]),
            };
            let name = si
                .map(|s| s.display_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| symbol_name(&o.symbol));
            local_defs.push(Def {
                symbol: o.symbol.clone(),
                name,
                kind,
                path: path.clone(),
                start_line: start + 1,
                end_line: end + 1,
                doc: si.and_then(|s| s.documentation.first().cloned()),
            });
        }
        // documents without enclosing ranges: extend each def to the line before the next one
        local_defs.sort_by_key(|d| d.start_line);
        for i in 0..local_defs.len() {
            if local_defs[i].end_line == local_defs[i].start_line
                && let Some(n) = local_defs.get(i + 1)
            {
                local_defs[i].end_line = (n.start_line - 1).max(local_defs[i].start_line);
            }
        }
        let scopes: Vec<&Def> = local_defs
            .iter()
            .filter(|d| callable(&d.symbol, &d.kind))
            .collect();
        for o in &d.occurrences {
            if is_local(&o.symbol) || o.range.len() < 3 || o.symbol_roles & 1 == 1 {
                continue;
            }
            let line = o.range[0] + 1;
            g.refs.push(Ref {
                symbol: o.symbol.clone(),
                path: path.clone(),
                line,
                col: o.range[1],
            });
            let ck = kinds.get(&o.symbol).copied().unwrap_or("symbol");
            if callable(&o.symbol, ck)
                && let Some(c) = scopes
                    .iter()
                    .filter(|d| d.start_line <= line && line <= d.end_line && d.symbol != o.symbol)
                    .min_by_key(|d| d.end_line - d.start_line)
            {
                g.calls.push(Call {
                    caller: c.symbol.clone(),
                    callee: o.symbol.clone(),
                    path: path.clone(),
                    line,
                });
            }
        }
        g.defs.extend(local_defs);
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occ(line: i32, sym: &str, roles: i32, enc: Vec<i32>) -> Occurrence {
        Occurrence {
            range: vec![line, 0, 3],
            symbol: sym.into(),
            symbol_roles: roles,
            enclosing_range: enc,
        }
    }

    #[test]
    fn names() {
        assert_eq!(
            symbol_name("rust-analyzer cargo muninn-core 0.1.0 pg/PgStore#connect()."),
            "connect"
        );
        assert_eq!(
            symbol_name("scip-typescript npm x 1.0 src/`a.ts`/Foo#bar()."),
            "bar"
        );
    }

    #[test]
    fn defs_refs_and_call_edges() {
        let a = "s cargo p 1 m/a().";
        let b = "s cargo p 1 m/b().";
        let ix = Index {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ(0, a, 1, vec![0, 0, 4, 1]),
                    occ(1, b, 0, vec![]),
                    occ(6, b, 1, vec![6, 0, 8, 1]),
                    occ(7, "local 3", 0, vec![]),
                ],
                symbols: vec![
                    SymbolInfo {
                        symbol: a.into(),
                        kind: 17,
                        ..Default::default()
                    },
                    SymbolInfo {
                        symbol: b.into(),
                        kind: 17,
                        ..Default::default()
                    },
                ],
            }],
        };
        let g = graph(&ix, "crate/");
        assert_eq!(g.defs.len(), 2);
        assert_eq!(g.defs[0].path, "crate/src/m.rs");
        assert_eq!(
            (
                g.defs[0].name.as_str(),
                g.defs[0].start_line,
                g.defs[0].end_line
            ),
            ("a", 1, 5)
        );
        assert_eq!(g.refs.len(), 1);
        assert_eq!(
            g.calls,
            vec![Call {
                caller: a.into(),
                callee: b.into(),
                path: "crate/src/m.rs".into(),
                line: 2
            }]
        );
    }
}
