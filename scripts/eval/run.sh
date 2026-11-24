#!/usr/bin/env bash
# Run every scripts/eval/cases-*.tsv against indexd and print the summary.
#
#   scripts/eval/run.sh [extra muninn eval flags]   # default: --mode all -k 10
#   scripts/eval/run.sh --json /tmp/eval.json -v
#   scripts/eval/run.sh --mode hybrid --min-recall 0.6   # CI gate
#
# MUNINN_INDEXD_URL defaults to http://127.0.0.1:7410; MUNINN_INDEXD_TOKEN is read from
# ~/.config/muninn/index.env when unset. MUNINN_BIN overrides the CLI binary (default: build
# and run it with cargo).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
if [[ -z "${MUNINN_INDEXD_TOKEN:-}" && -f ~/.config/muninn/index.env ]]; then
  MUNINN_INDEXD_TOKEN=$(. ~/.config/muninn/index.env && printf %s "${MUNINN_INDEXD_TOKEN:-}")
  export MUNINN_INDEXD_TOKEN
fi
args=("$@")
[[ " ${args[*]} " == *" --mode "* ]] || args+=(--mode all)
[[ " ${args[*]} " == *" -k "* ]] || args+=(-k 10)
if [[ -n "${MUNINN_BIN:-}" ]]; then
  bin=("$MUNINN_BIN")
else
  bin=(cargo run -q --release --manifest-path "$root/Cargo.toml" -p muninn-cli --)
fi
cases=()
for f in "$here"/cases-*.tsv; do
  cases+=("$f")
done
exec "${bin[@]}" eval "${cases[@]}" "${args[@]}"
