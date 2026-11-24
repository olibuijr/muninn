#!/bin/sh
# Enroll bare repo mirrors: install the push hook, then index HEAD, one repo at a time.
#   backfill.sh [REPO...]     only the named mirrors (enroll or reindex); without names, every mirror
# MUNINN_BACKFILL_MISSING=1 only enrolls mirrors that have no hook yet (muninn-enroll.timer runs that).
# Only repos in MUNINN_INDEX_ONLY are indexed when it is set; indexd skips the others.
ENVF=${MUNINN_ENV_FILE:-$HOME/.config/muninn/index.env}
setting() { sed -n "s/^$1=//p" "$ENVF" | tail -1; }
REPOS=${MUNINN_REPOS_DIR:-$(setting MUNINN_REPOS_DIR)}; REPOS=${REPOS:-$HOME/.local/share/muninn/repos}
URL=${MUNINN_INDEXD_URL:-$(setting MUNINN_INDEXD_URL)}; URL=${URL:-http://127.0.0.1:7410}
SECRET=$(setting MUNINN_WEBHOOK_SECRET)
HOOK=$(cd "$(dirname "$0")" && pwd)/hooks/reference-transaction
if [ $# -gt 0 ]; then set -- $(for r in "$@"; do echo "$REPOS/$r.git"; done); else set -- "$REPOS"/*.git; fi
for d in "$@"; do
  repo=$(basename "$d" .git)
  [ -d "$d" ] || { echo "$repo: no mirror at $d"; continue; }
  [ -n "$MUNINN_BACKFILL_MISSING" ] && [ -e "$d/hooks/reference-transaction" ] && continue
  ln -sfn "$HOOK" "$d/hooks/reference-transaction"
  ref=$(git -C "$d" symbolic-ref -q HEAD) || continue
  after=$(git -C "$d" rev-parse -q --verify "$ref") || continue
  body=$(printf '{"ref":"%s","before":"","after":"%s","repository":{"name":"%s"}}' "$ref" "$after" "$repo")
  sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$SECRET" | sed 's/^.* //')
  s=$(date +%s)
  out=$(curl -s -m 600 -X POST -H "X-Gitea-Signature: $sig" -H 'Content-Type: application/json' -d "$body" "$URL/webhook/push")
  echo "$repo $out $(( $(date +%s) - s ))s"
  sleep 1
done
