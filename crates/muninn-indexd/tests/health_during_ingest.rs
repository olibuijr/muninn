use std::io::{Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

fn sh(dir: &std::path::Path, args: &[&str]) {
    let s = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(s.success());
}

fn get(addr: &str, path: &str) -> String {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").unwrap();
    let mut b = String::new();
    s.read_to_string(&mut b).unwrap();
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_fast_during_large_ingest() {
    let tmp = std::env::temp_dir().join(format!("indexd-ingest-{}", std::process::id()));
    let repo = tmp.join("big");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    for i in 0..4000 {
        let body: String = (0..40)
            .map(|j| format!("fn f{i}_{j}() {{ let x = {j}; }}\n"))
            .collect();
        std::fs::write(repo.join(format!("f{i}.rs")), body).unwrap();
    }
    sh(&repo, &["add", "."]);
    sh(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "x",
        ],
    );
    let head = String::from_utf8(
        Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let head = head.trim().to_string();

    let app = muninn_indexd::App::new(tmp.clone(), None);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(axum::serve(l, muninn_indexd::router(app.clone())).into_future());

    let a = app.clone();
    let ingest = tokio::task::spawn_blocking(move || a.index_push("big", "main", None, &head));
    let mut worst = Duration::ZERO;
    let mut n = 0;
    while !ingest.is_finished() {
        let a = addr.clone();
        let t = Instant::now();
        let r = tokio::task::spawn_blocking(move || get(&a, "/healthz"))
            .await
            .unwrap();
        worst = worst.max(t.elapsed());
        assert!(r.ends_with("ok"), "{r}");
        let a = addr.clone();
        let r = tokio::task::spawn_blocking(move || get(&a, "/readyz"))
            .await
            .unwrap();
        assert!(r.contains("ingest"), "{r}");
        n += 1;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ingest.await.unwrap();
    std::fs::remove_dir_all(&tmp).ok();
    assert!(n > 3, "ingest too fast to exercise ({n} probes)");
    assert!(worst < Duration::from_millis(50), "worst healthz {worst:?}");
}
