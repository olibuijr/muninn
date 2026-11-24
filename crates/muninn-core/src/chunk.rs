use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Language, Node, Parser};

pub const WINDOW: usize = 60;
pub const OVERLAP: usize = 10;
/// Embed server runs `-ub 1024`; 2500 bytes is a conservative (~2.5 B/token) bound under that.
pub const MAX_CHUNK_BYTES: usize = 2500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub path: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub content_hash: String,
}

pub fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn language(path: &str) -> Option<(Language, &'static str)> {
    let ext = path.rsplit('.').next()?;
    Some(match ext {
        "rs" => (tree_sitter_rust::LANGUAGE.into(), "rust"),
        "ts" => (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), "ts"),
        "tsx" => (tree_sitter_typescript::LANGUAGE_TSX.into(), "ts"),
        "toml" => (tree_sitter_toml_ng::LANGUAGE.into(), "toml"),
        "yaml" | "yml" => (tree_sitter_yaml::LANGUAGE.into(), "yaml"),
        "md" => (tree_sitter_md::LANGUAGE.into(), "md"),
        "swift" => (tree_sitter_swift::LANGUAGE.into(), "swift"),
        "py" | "pyi" => (tree_sitter_python::LANGUAGE.into(), "python"),
        "sh" | "bash" => (tree_sitter_bash::LANGUAGE.into(), "bash"),
        "js" | "mjs" | "cjs" | "jsx" => (tree_sitter_javascript::LANGUAGE.into(), "js"),
        "go" => (tree_sitter_go::LANGUAGE.into(), "go"),
        "json" => (tree_sitter_json::LANGUAGE.into(), "json"),
        "c" | "h" => (tree_sitter_c::LANGUAGE.into(), "c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => (tree_sitter_cpp::LANGUAGE.into(), "cpp"),
        _ => return None,
    })
}

const SYMBOL_KINDS: &[&str] = &[
    "function_item",
    "struct_item",
    "enum_item",
    "trait_item",
    "impl_item",
    "mod_item",
    "const_item",
    "static_item",
    "type_item",
    "macro_definition",
    "function_declaration",
    "class_declaration",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
    "method_definition",
    "lexical_declaration",
    "export_statement",
    "table",
    "table_array_element",
    "block_mapping_pair",
    "section",
    // ts/js
    "generator_function_declaration",
    "variable_declaration",
    // swift (class_declaration also covers struct/enum/actor/extension)
    "protocol_declaration",
    "property_declaration",
    "init_declaration",
    "deinit_declaration",
    "subscript_declaration",
    "typealias_declaration",
    "protocol_function_declaration",
    // python / bash / c / cpp
    "function_definition",
    "class_definition",
    "decorated_definition",
    // go
    "method_declaration",
    "type_declaration",
    "const_declaration",
    "var_declaration",
    // c / cpp
    "struct_specifier",
    "union_specifier",
    "class_specifier",
    "enum_specifier",
    "namespace_definition",
    "type_definition",
    "template_declaration",
    // json (only counted when lang == "json", see is_symbol)
    "pair",
];

fn is_symbol(node: Node, lang: &str) -> bool {
    let kind = node.kind();
    if !SYMBOL_KINDS.contains(&kind) {
        return false;
    }
    match kind {
        "pair" => lang == "json",
        // `struct foo *p;` mentions a type without defining it
        "struct_specifier" | "union_specifier" | "class_specifier" | "enum_specifier" => {
            node.child_by_field_name("body").is_some()
        }
        _ => true,
    }
}

/// C/C++ names sit at the bottom of a declarator chain
/// (pointer_declarator -> function_declarator -> identifier / qualified_identifier).
fn declarator_name(node: Node, src: &[u8]) -> Option<String> {
    let mut d = node.child_by_field_name("declarator")?;
    for _ in 0..16 {
        let next = d.child_by_field_name("declarator").or_else(|| {
            (d.kind().ends_with("declarator") && d.named_child_count() > 0)
                .then(|| d.named_child(0))
                .flatten()
        });
        match next {
            Some(n) => d = n,
            None => break,
        }
    }
    d.utf8_text(src).ok().map(|s| s.to_string())
}

fn name_of(node: Node, src: &[u8]) -> Option<String> {
    let text = |n: Node| n.utf8_text(src).ok().map(|s| s.to_string());
    if let Some(n) = node.child_by_field_name("name") {
        return text(n);
    }
    let named = |kinds: &[&str]| {
        let mut c = node.walk();
        node.named_children(&mut c)
            .find(|n| kinds.contains(&n.kind()))
    };
    match node.kind() {
        "function_definition" | "type_definition" => declarator_name(node, src),
        "template_declaration" => named(&[
            "function_definition",
            "class_specifier",
            "struct_specifier",
            "declaration",
            "alias_declaration",
            "concept_definition",
        ])
        .and_then(|d| name_of(d, src)),
        "declaration" => declarator_name(node, src),
        "decorated_definition" => node
            .child_by_field_name("definition")
            .and_then(|d| name_of(d, src)),
        "type_declaration" => named(&["type_spec", "type_alias"]).and_then(|d| name_of(d, src)),
        "const_declaration" => named(&["const_spec"]).and_then(|d| name_of(d, src)),
        "var_declaration" => named(&["var_spec", "var_spec_list"]).and_then(|d| {
            if d.kind() == "var_spec_list" {
                d.named_child(0).and_then(|v| name_of(v, src))
            } else {
                name_of(d, src)
            }
        }),
        "pair" => node
            .child_by_field_name("key")
            .and_then(text)
            .map(|s| s.trim_matches('"').to_string()),
        _ if node.child_by_field_name("type").is_some() => {
            node.child_by_field_name("type").and_then(text)
        }
        "export_statement" => node
            .child_by_field_name("declaration")
            .and_then(|d| name_of(d, src)),
        "lexical_declaration" | "variable_declaration" => {
            let mut c = node.walk();
            node.children(&mut c)
                .find(|n| n.kind() == "variable_declarator")
                .and_then(|d| d.child_by_field_name("name"))
                .and_then(text)
        }
        "table" | "table_array_element" => node.named_child(0).and_then(text),
        "block_mapping_pair" => node.child_by_field_name("key").and_then(text),
        "section" => node
            .named_child(0)
            .and_then(text)
            .map(|s| s.trim_start_matches('#').trim().to_string()),
        _ => None,
    }
}

fn collect(
    node: Node,
    src: &[u8],
    lang: &str,
    out: &mut Vec<(Option<String>, String, usize, usize)>,
    depth: usize,
) {
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        if is_symbol(ch, lang) {
            let end = ch.end_position();
            let (s, e) = (
                ch.start_position().row,
                if end.column == 0 && end.row > ch.start_position().row {
                    end.row - 1
                } else {
                    end.row
                },
            );
            // oversize containers (impl/trait/class/mod) are split into members
            // decorators belong to the class they wrap; descend into its body
            let target = if ch.kind() == "decorated_definition" {
                ch.child_by_field_name("definition").unwrap_or(ch)
            } else {
                ch
            };
            if e - s + 1 > WINDOW && depth < 3 && descend_ok(target.kind()) {
                let before = out.len();
                let body = target
                    .child_by_field_name("body")
                    .or_else(|| target.child_by_field_name("value"))
                    .unwrap_or(target);
                collect(body, src, lang, out, depth + 1);
                if out.len() > before {
                    continue;
                }
            }
            out.push((name_of(ch, src), ch.kind().to_string(), s, e));
        } else if (matches!(
            ch.kind(),
            "block_mapping" | "block_node" | "stream" | "document" | "document_node"
        ) || container(ch.kind(), lang)
            || depth == 0 && ch.kind() != "line_comment")
            && depth < 3
        {
            collect(ch, src, lang, out, depth + 1);
        }
    }
}

fn descend_ok(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "trait_item"
            | "mod_item"
            | "class_declaration"
            | "section"
            | "protocol_declaration"
            | "class_definition"
            | "class_specifier"
            | "struct_specifier"
            | "namespace_definition"
            | "template_declaration"
            | "pair"
    )
}

/// Non-symbol wrappers worth looking through at any depth (include guards, `extern "C" {}`).
fn container(kind: &str, lang: &str) -> bool {
    matches!(lang, "c" | "cpp")
        && matches!(
            kind,
            "preproc_ifdef"
                | "preproc_if"
                | "preproc_else"
                | "preproc_elif"
                | "linkage_specification"
                | "declaration_list"
        )
}

fn window(path: &str, lines: &[&str], from: usize, to: usize, out: &mut Vec<Chunk>) {
    let mut start = from;
    while start < to {
        let end = (start + WINDOW).min(to);
        push(path, None, "window", lines, start, end - 1, out);
        if end == to {
            break;
        }
        start = end - OVERLAP;
    }
}

fn push(
    path: &str,
    symbol: Option<String>,
    kind: &str,
    lines: &[&str],
    s: usize,
    e: usize,
    out: &mut Vec<Chunk>,
) {
    let text = lines[s..=e].join("\n");
    if text.trim().is_empty() {
        return;
    }
    let content_hash =
        sha_hex(format!("{path}\0{}\0{text}", symbol.as_deref().unwrap_or("")).as_bytes());
    out.push(Chunk {
        path: path.into(),
        symbol,
        kind: kind.into(),
        start_line: s + 1,
        end_line: e + 1,
        text,
        content_hash,
    });
}

/// Symbol-granularity chunks via tree-sitter; sliding window fallback for unknown languages,
/// parse failures and gaps between symbols (so every line is covered).
pub fn chunk_file(path: &str, text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    if lines.is_empty() {
        return out;
    }
    let parsed = language(path).and_then(|(lang, name)| {
        let mut p = Parser::new();
        p.set_language(&lang).ok()?;
        p.parse(text, None).map(|t| (t, name))
    });
    let Some((tree, lang)) = parsed else {
        window(path, &lines, 0, lines.len(), &mut out);
        return cap(out);
    };
    let mut syms = Vec::new();
    collect(tree.root_node(), text.as_bytes(), lang, &mut syms, 0);
    syms.sort_by_key(|s| (s.2, std::cmp::Reverse(s.3)));
    let mut cursor = 0;
    for (name, kind, s, e) in syms {
        if s < cursor {
            continue;
        }
        // A doc comment or attribute block directly above an item belongs to it: left in the gap
        // window it outranks the item itself for any query phrased like its documentation.
        let s = leading_comment_start(&lines, cursor, s);
        if s > cursor {
            window(path, &lines, cursor, s, &mut out);
        }
        let e = e.min(lines.len() - 1);
        if e - s + 1 > WINDOW * 2 {
            window(path, &lines, s, e + 1, &mut out);
            if let Some(c) = out.iter_mut().rev().take_while(|c| c.start_line > s).last() {
                c.symbol = name;
                c.kind = kind;
            }
        } else {
            push(path, name, &kind, &lines, s, e, &mut out);
        }
        cursor = e + 1;
    }
    if cursor < lines.len() {
        window(path, &lines, cursor, lines.len(), &mut out);
    }
    cap(out)
}

/// First line of the comment/attribute/decorator block that ends right above `s` (not before
/// `from`), or `s` when there is none. A blank line ends the block.
fn leading_comment_start(lines: &[&str], from: usize, s: usize) -> usize {
    let is_lead = |l: &str| {
        let t = l.trim_start();
        !t.is_empty()
            && (t.starts_with("///")
                || t.starts_with("//!")
                || t.starts_with("//")
                || t.starts_with("/*")
                || t.starts_with("* ")
                || t == "*"
                || t.starts_with("*/")
                || t.starts_with("#[")
                || t.starts_with("#!")
                || t.starts_with("# ")
                || t == "#"
                || t.starts_with("@")
                || t.starts_with("--"))
    };
    let mut start = s;
    while start > from && is_lead(lines[start - 1]) {
        start -= 1;
    }
    start
}

fn cap(chunks: Vec<Chunk>) -> Vec<Chunk> {
    let mut out = Vec::with_capacity(chunks.len());
    for c in chunks {
        if c.text.len() <= MAX_CHUNK_BYTES {
            out.push(c);
            continue;
        }
        let mut buf = String::new();
        let mut first = c.start_line;
        let mut line_no = c.start_line;
        let flush = |buf: &mut String, from: usize, to: usize, out: &mut Vec<Chunk>| {
            if buf.trim().is_empty() {
                buf.clear();
                return;
            }
            let text = std::mem::take(buf);
            let h = sha_hex(
                format!("{}\0{}\0{text}", c.path, c.symbol.as_deref().unwrap_or("")).as_bytes(),
            );
            out.push(Chunk {
                path: c.path.clone(),
                symbol: c.symbol.clone(),
                kind: c.kind.clone(),
                start_line: from,
                end_line: to,
                text,
                content_hash: h,
            });
        };
        for line in c.text.split('\n') {
            let mut rest = line;
            loop {
                let sep = usize::from(!buf.is_empty());
                if buf.len() + sep + rest.len() <= MAX_CHUNK_BYTES {
                    if sep == 1 {
                        buf.push('\n');
                    }
                    buf.push_str(rest);
                    break;
                }
                if !buf.is_empty() {
                    flush(
                        &mut buf,
                        first,
                        line_no.saturating_sub(1).max(first),
                        &mut out,
                    );
                    first = line_no;
                    continue;
                }
                let mut n = MAX_CHUNK_BYTES;
                while !rest.is_char_boundary(n) {
                    n -= 1;
                }
                buf.push_str(&rest[..n]);
                flush(&mut buf, first, line_no, &mut out);
                first = line_no;
                rest = &rest[n..];
            }
            line_no += 1;
        }
        flush(&mut buf, first, c.end_line, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_comments_and_attributes_stay_with_their_item() {
        let src = "use std::io;\n\n/// Adds one.\n/// Twice documented.\n#[inline]\nfn alpha() -> u32 { 1 }\n\n// trailing note\n\nstruct Beta;\n";
        let c = chunk_file("a.rs", src);
        let alpha = c
            .iter()
            .find(|c| c.symbol.as_deref() == Some("alpha"))
            .unwrap();
        assert_eq!(alpha.start_line, 3, "{c:?}");
        assert!(alpha.text.starts_with("/// Adds one."));
        assert!(alpha.text.contains("#[inline]"));
        let beta = c
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Beta"))
            .unwrap();
        assert_eq!(
            beta.start_line, 10,
            "blank line separates the stray comment"
        );
        let py = chunk_file(
            "a.py",
            "import os\n\n# helper\n@decorator\ndef f():\n    pass\n",
        );
        let f = py
            .iter()
            .find(|c| c.symbol.as_deref() == Some("f"))
            .unwrap();
        assert_eq!(f.start_line, 3, "{py:?}");
    }

    #[test]
    fn rust_symbols() {
        let src = "use std::io;\n\nfn alpha() -> u32 { 1 }\n\nstruct Beta { x: u8 }\n\nimpl Beta {\n    fn gamma(&self) {}\n}\n";
        let c = chunk_file("a.rs", src);
        let names: Vec<_> = c.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(names, vec!["alpha", "Beta", "Beta"]);
        assert!(
            c.iter().any(|c| c.kind == "window"),
            "gap (use line) covered"
        );
    }

    #[test]
    fn ts_symbols() {
        let c = chunk_file(
            "a.ts",
            "export function foo() { return 1 }\nclass Bar { m() {} }\nconst baz = () => 2;\n",
        );
        let names: Vec<_> = c.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(names, vec!["foo", "Bar", "baz"]);
    }

    #[test]
    fn toml_and_md() {
        let c = chunk_file(
            "Cargo.toml",
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
        );
        assert!(
            c.iter()
                .any(|c| c.symbol.as_deref() == Some("dependencies")),
            "{c:?}"
        );
        let m = chunk_file("R.md", "# Title\ntext\n\n## Sub\nmore\n");
        assert!(
            m.iter().any(|c| c.symbol.as_deref() == Some("Title")),
            "{m:?}"
        );
    }

    #[test]
    fn fallback_window_overlaps() {
        let text = (0..130)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let c = chunk_file("x.txt", &text);
        assert_eq!(c.len(), 3);
        assert_eq!(c[1].start_line, 51);
    }

    #[test]
    fn hash_stable_and_content_addressed() {
        let a = chunk_file("a.rs", "fn f() {}\n");
        let b = chunk_file("a.rs", "fn f() {}\n");
        assert_eq!(a[0].content_hash, b[0].content_hash);
    }

    #[test]
    fn chunks_respect_token_cap() {
        let big = format!("fn f() {{\n{}\n}}\n", "let x = 1; ".repeat(2000));
        let cs = chunk_file("a.rs", &big);
        assert!(cs.len() > 1);
        assert!(cs.iter().all(|c| c.text.len() <= MAX_CHUNK_BYTES));
        let cs = chunk_file("b.txt", &"word ".repeat(5000));
        assert!(cs.iter().all(|c| c.text.len() <= MAX_CHUNK_BYTES));
    }

    /// Chunks `src`, asserts every non-blank line is covered and every chunk fits the cap,
    /// and returns the symbol names in order.
    fn covered(path: &str, src: &str) -> Vec<String> {
        let cs = chunk_file(path, src);
        for (i, l) in src.lines().enumerate() {
            if l.trim().is_empty() {
                continue;
            }
            assert!(
                cs.iter().any(|c| c.start_line <= i + 1 && i < c.end_line),
                "{path}: line {} uncovered: {l:?}\n{cs:#?}",
                i + 1
            );
        }
        assert!(cs.iter().all(|c| c.text.len() <= MAX_CHUNK_BYTES));
        cs.into_iter().filter_map(|c| c.symbol).collect()
    }

    fn big_body(stmt: &str) -> String {
        (0..70)
            .map(|i| stmt.replace('N', &i.to_string()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn swift_symbols() {
        let src = "import Foundation\n\nprotocol Shape { func area() -> Double }\n\nstruct Sq: Shape {\n    var side: Double\n    func area() -> Double { side * side }\n}\n\nenum Dir { case n, s }\n\nextension Sq {\n    init(s: Double) { side = s }\n}\n\nfunc top() {}\nlet answer = 42\n";
        assert_eq!(
            covered("a.swift", src),
            vec!["Shape", "Sq", "Dir", "Sq", "top", "answer"]
        );
        // large types are split into members
        let big = format!(
            "final class Big {{\n    var a = 1\n    init() {{}}\n    func f() {{\n{}\n    }}\n    func g() {{}}\n}}\n",
            big_body("        print(N)")
        );
        assert_eq!(covered("b.swift", &big), vec!["a", "init", "f", "g"]);
    }

    /// Real-world shape (a 9 KB Swift math file):
    /// top-level structs, extensions and funcs each become one chunk.
    #[test]
    fn swift_real_file() {
        let src = include_str!("../tests/fixtures/MGMath.swift");
        let names = covered("MGMath.swift", src);
        for want in ["MGVec2", "MGPoint3", "MGQuantPos", "MGBounds"] {
            assert!(names.iter().any(|n| n == want), "{want} missing: {names:?}");
        }
        let cs = chunk_file("MGMath.swift", src);
        // only the header comment and doc comments between members are left to windows
        let window_lines: usize = cs
            .iter()
            .filter(|c| c.kind == "window")
            .map(|c| c.end_line - c.start_line + 1)
            .sum();
        assert!(window_lines * 3 < src.lines().count(), "{cs:#?}");
        assert!(
            names.iter().any(|n| n == "pointInPolygon"),
            "enum members split"
        );
    }

    #[test]
    fn python_symbols() {
        let src = format!(
            "import os\n\n@dataclass\nclass Small:\n    x: int\n\ndef helper(a):\n    return a\n\nclass Big(Base):\n    \"\"\"doc\"\"\"\n\n    @property\n    def p(self):\n        return 1\n\n    def run(self):\n{}\n\nasync def later():\n    pass\n",
            big_body("        x = N")
        );
        let names = covered("a.py", &src);
        assert_eq!(names, vec!["Small", "helper", "p", "run", "later"]);
        let c = chunk_file("a.py", &src);
        let small = c
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Small"))
            .unwrap();
        assert!(
            small.text.starts_with("@dataclass"),
            "decorator kept: {small:?}"
        );
    }

    #[test]
    fn bash_symbols() {
        let src = "#!/bin/sh\nset -e\n\nusage() {\n  echo hi\n}\n\nfunction main {\n  usage\n}\n\nmain \"$@\"\n";
        assert_eq!(covered("x.sh", src), vec!["usage", "main"]);
    }

    #[test]
    fn js_symbols() {
        let src = format!(
            "import x from 'y';\nexport function foo() {{ return 1 }}\nfunction* gen() {{}}\nvar old = 1;\nconst arrow = () => 2;\nclass Big {{\n  constructor() {{}}\n  m() {{\n{}\n  }}\n}}\nmodule.exports = {{ foo }};\n",
            big_body("    let a = N;")
        );
        assert_eq!(
            covered("a.mjs", &src),
            vec!["foo", "gen", "old", "arrow", "constructor", "m"]
        );
        assert_eq!(covered("c.jsx", "const A = () => <div/>;\n"), vec!["A"]);
    }

    #[test]
    fn go_symbols() {
        let src = "package main\n\nimport \"fmt\"\n\nconst Pi = 3.14\n\nvar (\n\tA = 1\n\tB = 2\n)\n\ntype T struct {\n\tX int\n}\n\nfunc (t *T) Get() int { return t.X }\n\nfunc main() {\n\tfmt.Println(Pi)\n}\n";
        assert_eq!(covered("m.go", src), vec!["Pi", "A", "T", "Get", "main"]);
    }

    #[test]
    fn json_symbols() {
        let src = format!(
            "{{\n  \"name\": \"x\",\n  \"scripts\": {{\n    \"build\": \"tsc\"\n  }},\n  \"big\": {{\n{}\n    \"last\": 0\n  }},\n  \"list\": [\n{}\n    0\n  ]\n}}\n",
            big_body("    \"kN\": N,"),
            (0..200)
                .map(|i| format!("    {i},"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let names = covered("package.json", &src);
        assert_eq!(&names[..3], &["name", "scripts", "k0"]);
        assert!(names.iter().any(|n| n == "last"));
        // huge arrays still fall back to windows, the first one labelled with the key
        let c = chunk_file("package.json", &src);
        let list: Vec<_> = c
            .iter()
            .filter(|c| c.text.contains("    150,") || c.symbol.as_deref() == Some("list"))
            .collect();
        assert!(
            list.iter().any(|c| c.symbol.as_deref() == Some("list")),
            "{list:?}"
        );
        assert!(list.iter().any(|c| c.kind == "window"), "{list:?}");
    }

    /// Real-world shape (an asset recolor table): one chunk per top-level key.
    /// Also observed on a 93 KB level-data JSON file
    /// (1349 lines, scalar header keys + a "holes" array): the scalar keys became named
    /// one-line chunks and "holes" was windowed, the first window labelled "holes".
    #[test]
    fn json_real_file() {
        let src = include_str!("../tests/fixtures/recolor.json");
        let names = covered("recolor.json", src);
        assert_eq!(
            &names[..4],
            &["sili", "flag", "cairn_stone", "rock_large_a"]
        );
    }

    #[test]
    fn c_symbols() {
        let src = "#ifndef A_H\n#define A_H\n#include <stdio.h>\n\nstruct point { int x, y; };\n\ntypedef struct { int v; } box_t;\n\nenum color { RED, GREEN };\n\nstruct point *mk(void);\n\nstatic int *get(int a) {\n  return 0;\n}\n\nint main(void) {\n  return 0;\n}\n#endif\n";
        assert_eq!(
            covered("a.h", src),
            vec!["point", "box_t", "color", "get", "main"]
        );
    }

    #[test]
    fn cpp_symbols() {
        let src = format!(
            "#include <vector>\nnamespace ns {{\nclass Widget {{\n public:\n  int size() const {{ return 1; }}\n}};\n\nint Widget::other() {{\n{}\n  return 0;\n}}\n\ntemplate <typename T>\nT twice(T v) {{ return v + v; }}\n\nstruct S {{ int a; }};\n}}  // namespace ns\nextern \"C\" {{\nvoid cfun() {{}}\n}}\n",
            big_body("  int vN = N;")
        );
        assert_eq!(
            covered("w.cpp", &src),
            vec!["Widget", "Widget::other", "twice", "S", "cfun"]
        );
    }
}
