# Contributing

Muninn is free and open source under the [MIT licence](LICENSE). Use it, fork it, change it and
ship it. Issues and pull requests are welcome.

- **Bugs and ideas:** open an issue with what you ran, what you expected and what happened.
  `muninn doctor` and `journalctl --user -u muninn-indexd` (or `-memoryd`, `-mcp`) help.
- **Pull requests:** keep each one to a single change and add a test when behaviour changes. Run
  `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings` before you push, and
  `cargo fmt` on the files you touched.
- **Ranking changes** (chunking, fusion weights, rerank depth, expansion) need numbers: run
  `muninn eval` on cases from your own repos before and after (`muninn eval --compare`) and put
  the deltas in the pull request.
- **Security issues:** report them privately through GitHub's security advisories rather than a
  public issue.
- **The website** is the static page in [site/](site/), published to GitHub Pages on every push
  to `main`.
