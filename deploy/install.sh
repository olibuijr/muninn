#!/usr/bin/env bash
# Install or upgrade Muninn for this user: binaries in ~/.local/bin, helper scripts in
# ~/.local/share/muninn, systemd user units, then (re)start the services that are enabled.
# Works from a release tarball (binaries in ./bin) or from the repository (builds them).
#   ./install.sh            install, enable indexd, mcp and memoryd
#   ./install.sh --no-start install files only
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
bins="muninn muninn-indexd muninn-mcp muninn-memoryd muninn-indexweb muninn-watch muninn-scip"
if [[ -d "$here/bin" ]]; then
  src="$here/bin"   # release tarball
else
  (cd "$here/.." && cargo build --release --locked)
  src="$here/../target/release"
fi
for b in $bins; do install -Dm755 "$src/$b" ~/.local/bin/$b; done
install -Dm755 "$here/backfill.sh" ~/.local/share/muninn/backfill.sh
install -Dm755 "$here/scip-reindex.sh" ~/.local/share/muninn/scip-reindex.sh
install -Dm755 "$here/hooks/reference-transaction" ~/.local/share/muninn/hooks/reference-transaction
mkdir -p ~/.config/systemd/user ~/.config/muninn
install -m644 "$here"/systemd/muninn-* ~/.config/systemd/user/
systemctl --user daemon-reload
[[ "${1:-}" == --no-start ]] && exit 0
# The services refuse to start (and systemd restarts them in a loop) without these, so check first.
check_env() {
  local f=~/.config/muninn/index.env k v missing=()
  if [[ ! -s $f ]]; then
    echo "write $f first (start from index.env.example), then run this again" >&2
    return 1
  fi
  setting() { sed -n "s/^$1=//p" "$f" | tail -1; }
  for k in MUNINN_PG_URL MUNINN_EMBED_URL MUNINN_RERANK_URL MUNINN_REPOS_DIR MUNINN_INDEXD_TOKEN \
           MUNINN_WEBHOOK_SECRET MUNINN_TOKENS_FILE; do
    v=$(setting $k)
    [[ -z $v || $v == *CHANGE_ME* ]] && missing+=("$k")
  done
  if (( ${#missing[@]} )); then
    echo "set ${missing[*]} in $f (see index.env.example), then run this again" >&2
    return 1
  fi
  v=$(setting MUNINN_TOKENS_FILE)
  if [[ ! -s ${v/#\~/$HOME} ]]; then
    echo "MUNINN_TOKENS_FILE ($v) does not exist yet: create it (README.md, Tokens and scopes), then run this again" >&2
    return 1
  fi
}
check_env || exit 1
systemctl --user enable --now muninn-indexd muninn-mcp muninn-memoryd muninn-lifecycle.timer muninn-enroll.timer
for u in muninn-indexd muninn-mcp muninn-memoryd muninn-indexweb muninn-watch; do
  systemctl --user is-enabled -q $u 2>/dev/null && systemctl --user restart $u
done
sleep 2
curl -fsS 127.0.0.1:7410/healthz && echo " indexd ok"
