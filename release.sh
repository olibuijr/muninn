#!/usr/bin/env bash
# Build static Linux binaries and publish them as a GitHub release (no CI involved).
#
#   ./release.sh          build, package and upload release v<Cargo.toml version>
#   ./release.sh build    only build and package into dist/
#
# Linked statically against musl, so the binaries run on any x86_64 Linux. The asset name has no
# version, so this link serves the newest:
#   https://github.com/olibuijr/muninn/releases/latest/download/muninn-x86_64-linux.tar.gz
# Needs: rustup target x86_64-unknown-linux-musl, musl-gcc, gh (logged in).
set -euo pipefail
cd -- "$(dirname -- "$0")"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag=v$version
target=x86_64-unknown-linux-musl
name=muninn-x86_64-linux
bins="muninn muninn-indexd muninn-mcp muninn-memoryd muninn-indexweb muninn-watch muninn-scip"

die() { echo "release: $*" >&2; exit 1; }

build() {
    grep -q "^## \[$version\]" CHANGELOG.md || die "CHANGELOG.md has no section for $version"
    CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked --target $target
    [[ "$(target/$target/release/muninn --version)" == "muninn $version" ]] || die "binary does not report $version"
    rm -rf "dist/$name" "dist/$name.tar.gz"
    mkdir -p "dist/$name/bin" "dist/$name/systemd" "dist/$name/hooks"
    for b in $bins; do cp "target/$target/release/$b" "dist/$name/bin/"; done
    cp deploy/install.sh deploy/client.sh deploy/backfill.sh deploy/scip-reindex.sh deploy/index.env.example \
       deploy/README.md LICENSE "dist/$name/"
    cp README.md "dist/$name/README-muninn.md"
    cp deploy/systemd/* "dist/$name/systemd/"
    cp deploy/hooks/reference-transaction "dist/$name/hooks/"
    tar -C dist --owner=0 --group=0 -czf "dist/$name.tar.gz" "$name"
    (cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
    echo "packaged dist/$name.tar.gz ($(du -h "dist/$name.tar.gz" | cut -f1))"
}

notes() {
    awk -v v="$version" '$0 ~ "^## \\[" v "\\]" {on=1; next} /^## \[/ {on=0} on' CHANGELOG.md
    cat <<EOF

## Install

\`\`\`sh
curl -LO https://github.com/olibuijr/muninn/releases/latest/download/$name.tar.gz
tar xzf $name.tar.gz && cd $name
mkdir -p ~/.config/muninn && cp index.env.example ~/.config/muninn/index.env   # fill it in
# create ~/.config/muninn/tokens.json (README.md, Tokens and scopes), then:
./install.sh                                      # the services, as systemd user units
MUNINN_HOST=http://SERVER ./client.sh             # each machine with Claude Code or Codex
\`\`\`

Static x86_64 Linux binaries (musl). Needs Postgres with pgvector and an OpenAI-compatible
embedding and rerank server. Check the download with $name.tar.gz.sha256.
EOF
}

publish() {
    [[ -z "$(git status --porcelain -- crates Cargo.toml Cargo.lock skills deploy)" ]] || die "commit the source first"
    git push -q origin HEAD
    git rev-parse -q --verify "refs/tags/$tag" >/dev/null || git tag -a "$tag" -m "Muninn $version"
    git push -q origin "$tag"
    if gh release view "$tag" >/dev/null 2>&1; then
        gh release upload "$tag" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256" --clobber
    else
        gh release create "$tag" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256" --title "Muninn $version" --notes "$(notes)" --latest
    fi
    echo "released $tag: https://github.com/olibuijr/muninn/releases/tag/$tag"
}

case ${1:-release} in
    build) build ;;
    release) build; publish ;;
    *) die "unknown command $1; see the top of release.sh" ;;
esac
