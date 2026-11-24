use muninn_core::PgStore;
use serde_json::{Value, json};
use std::collections::HashMap;

pub const E_CONTAINS: u8 = 0;
pub const E_SIMILAR: u8 = 1;

struct Nodes {
    label: Vec<String>,
    kind: Vec<u8>,
    repo: Vec<u32>,
    path: Vec<String>,
    line: Vec<i32>,
    size: Vec<f32>,
    parent: Vec<i32>,
    x: Vec<f32>,
    y: Vec<f32>,
    ids: HashMap<String, u32>,
}

impl Nodes {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        id: String,
        label: String,
        kind: u8,
        repo: u32,
        path: String,
        line: i32,
        size: f32,
        parent: i32,
    ) -> u32 {
        if let Some(&i) = self.ids.get(&id) {
            return i;
        }
        let i = self.label.len() as u32;
        self.ids.insert(id, i);
        self.label.push(label);
        self.kind.push(kind);
        self.repo.push(repo);
        self.path.push(path);
        self.line.push(line);
        self.size.push(size);
        self.parent.push(parent);
        self.x.push(0.0);
        self.y.push(0.0);
        i
    }
}

const GOLDEN: f32 = 2.399_963_2;

/// Phyllotaxis disc of `n` points, spacing `sp`.
fn disc(n: usize, sp: f32) -> Vec<(f32, f32)> {
    (0..n)
        .map(|i| {
            let r = sp * ((i as f32) + 0.5).sqrt();
            let a = i as f32 * GOLDEN;
            (r * a.cos(), r * a.sin())
        })
        .collect()
}

/// Place circles of the given radii on a golden-angle spiral without overlap (gap `gap`).
fn pack(radii: &[f32], gap: f32) -> Vec<(f32, f32)> {
    let mut placed: Vec<(f32, f32, f32)> = vec![];
    for (slot, &rr) in radii.iter().enumerate() {
        let mut s = 0usize;
        if slot == 0 {
            placed.push((0.0, 0.0, rr));
            continue;
        }
        let (x, y) = loop {
            let d = gap.max(4.0) * 0.6 * (s as f32 + 0.5).sqrt() * 2.0 + rr * 0.5;
            let a = s as f32 * GOLDEN;
            let (x, y) = (d * a.cos(), d * a.sin());
            if placed
                .iter()
                .all(|&(px, py, pr)| ((x - px).powi(2) + (y - py).powi(2)).sqrt() > pr + rr + gap)
            {
                break (x, y);
            }
            s += 1;
        };
        placed.push((x, y, rr));
    }
    placed.into_iter().map(|p| (p.0, p.1)).collect()
}

pub fn build(store: &PgStore, scope: &str, limit: usize, k: i64, version: &str) -> Value {
    let t0 = std::time::Instant::now();
    let single = scope != "all";
    let repo_filter = single.then_some(scope);
    let mut files = store.graph_files(repo_filter);
    files.retain(|f| !f.0.starts_with("oh-my-pi"));
    let mut repos: Vec<String> = files
        .iter()
        .map(|f| f.0.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    repos.sort();
    let rix: HashMap<&str, u32> = repos
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i as u32))
        .collect();

    let mut n = Nodes {
        label: vec![],
        kind: vec![],
        repo: vec![],
        path: vec![],
        line: vec![],
        size: vec![],
        parent: vec![],
        x: vec![],
        y: vec![],
        ids: HashMap::new(),
    };
    let mut e: Vec<(u32, u32, u8, f32)> = vec![];

    let mut nfiles: HashMap<&str, usize> = HashMap::new();
    for f in &files {
        *nfiles.entry(f.0.as_str()).or_default() += 1;
    }
    for r in &repos {
        let c = nfiles[r.as_str()];
        n.add(
            format!("r:{r}"),
            r.clone(),
            0,
            rix[r.as_str()],
            String::new(),
            0,
            12.0 + (c as f32).sqrt(),
            -1,
        );
    }
    // files, largest first, capped so chunks still fit under `limit`
    let mut fs: Vec<&(String, String, i32)> = files.iter().collect();
    fs.sort_by_key(|f| std::cmp::Reverse(f.2));
    let per_repo = if single { usize::MAX } else { 250 };
    let file_cap = if single {
        limit / 4
    } else {
        limit.saturating_sub(repos.len())
    };
    let mut taken: HashMap<&str, usize> = HashMap::new();
    fs.retain(|f| {
        let t = taken.entry(f.0.as_str()).or_default();
        *t += 1;
        *t <= per_repo
    });
    fs.truncate(file_cap.max(1));
    for f in fs {
        let r = rix[f.0.as_str()];
        let rid = n.ids[&format!("r:{}", f.0)];
        let label = f.1.rsplit('/').next().unwrap_or(&f.1).to_string();
        let i = n.add(
            format!("f:{}:{}", f.0, f.1),
            label,
            1,
            r,
            f.1.clone(),
            0,
            2.0 + f.2 as f32,
            rid as i32,
        );
        e.push((rid, i, E_CONTAINS, 1.0));
    }

    let level;
    let cap_src = (limit as i64).min(20000);
    if single {
        level = "chunks";
        let budget = limit.saturating_sub(n.label.len()) as i64;
        for c in store.graph_chunks(scope, budget) {
            let Some(&fi) = n.ids.get(&format!("f:{}:{}", c.repo, c.path)) else {
                continue;
            };
            let label = c
                .symbol
                .clone()
                .unwrap_or_else(|| format!("{}:{}", c.kind, c.start));
            let r = n.repo[fi as usize];
            let i = n.add(
                format!("c:{}:{}", c.repo, c.hash),
                label,
                2,
                r,
                c.path.clone(),
                c.start,
                1.0 + (c.end - c.start).max(1) as f32,
                fi as i32,
            );
            e.push((fi, i, E_CONTAINS, 1.0));
        }
        let mut seen = std::collections::HashSet::new();
        for s in store.graph_similar(repo_filter, k, 0.55f64, cap_src) {
            let (Some(&a), Some(&b)) = (
                n.ids.get(&format!("c:{}:{}", s.a.0, s.a.1)),
                n.ids.get(&format!("c:{}:{}", s.b.0, s.b.1)),
            ) else {
                continue;
            };
            let key = (a.min(b), a.max(b));
            if a != b && seen.insert(key) {
                e.push((key.0, key.1, E_SIMILAR, s.sim));
            }
        }
    } else {
        level = "files";
        let mut agg: HashMap<(u32, u32), (f32, u32)> = HashMap::new();
        for s in store.graph_similar(None, k, 0.55f64, cap_src) {
            let (Some(&a), Some(&b)) = (
                n.ids.get(&format!("f:{}:{}", s.a.0, s.a.2)),
                n.ids.get(&format!("f:{}:{}", s.b.0, s.b.2)),
            ) else {
                continue;
            };
            if a != b {
                let v = agg.entry((a.min(b), a.max(b))).or_default();
                v.0 = v.0.max(s.sim);
                v.1 += 1;
            }
        }
        for ((a, b), (sim, _)) in agg {
            e.push((a, b, E_SIMILAR, sim));
        }
    }

    // layout: repo discs packed on a spiral; inside a repo, directory groups are packed, files sit in group discs.
    let nn = n.label.len();
    let mut kids: Vec<Vec<u32>> = vec![vec![]; nn];
    for i in 0..nn {
        if n.parent[i] >= 0 {
            kids[n.parent[i] as usize].push(i as u32);
        }
    }
    let repo_nodes: Vec<usize> = (0..nn).filter(|&i| n.kind[i] == 0).collect();
    struct Group {
        files: Vec<u32>,
        r: f32,
    }
    let mut repo_groups: Vec<Vec<Group>> = vec![];
    let mut repo_r: Vec<f32> = vec![];
    for &r in &repo_nodes {
        let mut by: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
        for &f in &kids[r] {
            let segs: Vec<&str> = n.path[f as usize].split('/').collect();
            let key = segs[..segs.len().saturating_sub(1).min(2)].join("/");
            by.entry(key).or_default().push(f);
        }
        let mut groups: Vec<Group> = by
            .into_values()
            .map(|mut files| {
                files.sort_by(|a, b| n.path[*a as usize].cmp(&n.path[*b as usize]));
                let r = 3.0 * (files.len() as f32 + 1.0).sqrt() + 3.0;
                Group { files, r }
            })
            .collect();
        groups.sort_by(|a, b| b.r.total_cmp(&a.r));
        let radii: Vec<f32> = groups.iter().map(|g| g.r).collect();
        let pos = pack(&radii, 4.0);
        let enc = pos
            .iter()
            .zip(&radii)
            .map(|(p, r)| (p.0 * p.0 + p.1 * p.1).sqrt() + r)
            .fold(10.0f32, f32::max);
        n.x[r] = 0.0;
        n.y[r] = 0.0;
        for (g, p) in groups.iter().zip(&pos) {
            let fp = disc(g.files.len(), 3.0);
            for (j, &f) in g.files.iter().enumerate() {
                n.x[f as usize] = p.0 + fp[j].0;
                n.y[f as usize] = p.1 + fp[j].1;
            }
        }
        // store group offsets relative to repo centre; chunks follow their file
        repo_groups.push(groups);
        repo_r.push(enc);
    }
    let pos = pack(&repo_r, 30.0);
    for (ri, &r) in repo_nodes.iter().enumerate() {
        let (cx, cy) = pos[ri];
        n.x[r] = cx;
        n.y[r] = cy;
        for g in &repo_groups[ri] {
            for &f in &g.files {
                n.x[f as usize] += cx;
                n.y[f as usize] += cy;
                let cl = &kids[f as usize];
                let cp = disc(cl.len(), 1.0);
                for (m, &c) in cl.iter().enumerate() {
                    n.x[c as usize] = n.x[f as usize] + cp[m].0;
                    n.y[c as usize] = n.y[f as usize] + cp[m].1;
                }
            }
        }
    }

    let mut deg = vec![0u32; nn];
    for &(a, b, _, _) in &e {
        deg[a as usize] += 1;
        deg[b as usize] += 1;
    }
    let r2 = |v: &[f32]| {
        v.iter()
            .map(|x| (x * 100.0).round() / 100.0)
            .collect::<Vec<_>>()
    };
    json!({
        "version": version, "scope": scope, "level": level, "k": k,
        "generated_ms": t0.elapsed().as_millis() as u64,
        "repos": repos,
        "n": {
            "label": n.label, "kind": n.kind, "repo": n.repo, "path": n.path, "line": n.line,
            "size": r2(&n.size), "deg": deg, "x": r2(&n.x), "y": r2(&n.y),
        },
        "e": {
            "s": e.iter().map(|x| x.0).collect::<Vec<_>>(),
            "t": e.iter().map(|x| x.1).collect::<Vec<_>>(),
            "type": e.iter().map(|x| x.2).collect::<Vec<_>>(),
            "w": e.iter().map(|x| (x.3 * 100.0).round() / 100.0).collect::<Vec<_>>(),
        },
    })
}
