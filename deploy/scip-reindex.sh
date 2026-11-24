#!/bin/bash
# Rust SCIP symbol reindex outside indexd. A rust-analyzer run over a large workspace needs several GB,
# more than indexd's cgroup allows, so this runs `muninn-scip run` per repo in its own systemd scope,
# one at a time under a lock (point MUNINN_SCIP_LOCK at your build lock so it never overlaps a big build).
# Usage: scip-reindex.sh [REPO...]   (default: $MUNINN_SCIP_REPOS)
# Env: MUNINN_SCIP_BIN, MUNINN_SCIP_MIN_AVAIL_GB (7), MUNINN_SCIP_BIG_REPOS + MUNINN_SCIP_MIN_AVAIL_BIG_GB (9),
#      MUNINN_SCIP_MEM_HIGH (5G), MUNINN_SCIP_MEM_MAX (6G), MUNINN_SCIP_LOCK, MUNINN_SCIP_LOCK_WAIT seconds (7200).
set -uo pipefail

if [ -z "${MUNINN_PG_URL:-}" ] && [ -r "$HOME/.config/muninn/index.env" ]; then
    set -a; . "$HOME/.config/muninn/index.env"; set +a
fi
bin="${MUNINN_SCIP_BIN:-$HOME/.local/bin/muninn-scip}"
lock="${MUNINN_SCIP_LOCK:-$HOME/.cache/muninn/scip.lock}"
min_avail="${MUNINN_SCIP_MIN_AVAIL_GB:-7}"
if [ $# -gt 0 ]; then repos=("$@"); else read -ra repos <<< "${MUNINN_SCIP_REPOS:-}"; fi
[ ${#repos[@]} -gt 0 ] || { echo "scip-reindex: name repos or set MUNINN_SCIP_REPOS"; exit 0; }

# Under systemd stdout already goes to the journal; from a terminal, copy it there too.
if [ -z "${JOURNAL_STREAM:-}" ] && command -v logger >/dev/null; then
    exec > >(tee >(logger -t muninn-scip)) 2>&1
fi
say() { echo "scip-reindex: $*"; }

[ -x "$bin" ] || { say "no muninn-scip binary at $bin (cargo build --release -p muninn-scip)"; exit 1; }
mkdir -p "$(dirname "$lock")"
exec 9>"$lock"
say "waiting for the build lock $lock"
flock -w "${MUNINN_SCIP_LOCK_WAIT:-7200}" 9 || { say "lock wait timed out; skipping"; exit 0; }

rc=0
for repo in "${repos[@]}"; do
    # Checked per repo, after the lock: a build that just ended may still be releasing memory.
    avail=$(free -g | awk '/^Mem:/ {print $7}')
    need="$min_avail"
    case " ${MUNINN_SCIP_BIG_REPOS:-} " in *" $repo "*) need="${MUNINN_SCIP_MIN_AVAIL_BIG_GB:-9}" ;; esac
    if [ "${avail:-0}" -lt "$need" ]; then
        say "skip $repo: ${avail} GB available, need ${need} GB"
        continue
    fi
    say "start $repo (${avail} GB available)"
    # background.slice, not app.slice: systemd-oomd kills app.slice members after 20 s at 50 % memory
    # pressure, which MemoryHigh throttling alone produces. The kernel limits below are the guard
    # instead; MemorySwapMax keeps a runaway run from swapping the machine to a crawl. 2 cargo jobs: rust-analyzer runs a
    # full `cargo check` of the workspace for build scripts and proc macros.
    MUNINN_SCIP_JOBS="${MUNINN_SCIP_JOBS:-2}" \
    systemd-run --user --scope --quiet --collect --slice=background.slice --unit="muninn-scip-$repo-$$" \
        -p MemoryHigh="${MUNINN_SCIP_MEM_HIGH:-5G}" -p MemoryMax="${MUNINN_SCIP_MEM_MAX:-6G}" \
        -p MemorySwapMax=1G -p CPUWeight=20 \
        ionice -c3 "$bin" run --repo "$repo"
    r=$?
    [ $r -eq 0 ] && say "done $repo" || { say "FAILED $repo (exit $r)"; rc=1; }
done
exit $rc
