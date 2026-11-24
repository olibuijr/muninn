#!/usr/bin/env bash
# Connect this machine's Claude Code and Codex to a Muninn server: MCP server, hooks and the skill.
#
#   MUNINN_HOST=http://server ./client.sh            token from MUNINN_TOKEN or a prompt
#   MUNINN_HOOK_REPOS='~/src/api,~/src/acme-*' ...    repos where grep waits for the index
#
# Writes ~/.config/muninn/client.env and ~/.config/muninn/token (0600). The token never goes into
# an agent's config: Claude Code fetches it through `muninn mcp-header` at connect time.
set -euo pipefail
host=${MUNINN_HOST:-http://127.0.0.1}
command -v muninn >/dev/null || { echo "install the muninn binary into PATH first" >&2; exit 1; }
mkdir -p ~/.config/muninn && chmod 700 ~/.config/muninn
if [[ ! -s ~/.config/muninn/token ]]; then
  tok=${MUNINN_TOKEN:-}
  if [[ -z $tok ]]; then read -rsp "Muninn token for this machine: " tok; echo; fi
  (umask 077 && printf '%s\n' "$tok" > ~/.config/muninn/token)
fi
cat > ~/.config/muninn/client.env <<EOF
MUNINN_INDEXD_URL=$host:7410
MUNINN_MEMORYD_URL=$host:7412
MUNINN_HOOK_REPOS=${MUNINN_HOOK_REPOS:-}
EOF
chmod 600 ~/.config/muninn/client.env

# Merge the muninn hooks into a Claude Code settings.json or a Codex hooks.json (same layout),
# replacing earlier muninn entries and keeping every other hook.
install_hooks() {
  python3 - "$1" "$2" <<'PY'
import json, os, sys
p, pre = os.path.expanduser(sys.argv[1]), sys.argv[2]
s = json.load(open(p)) if os.path.exists(p) and os.path.getsize(p) else {}
hooks = s.setdefault("hooks", {})
want = {"UserPromptSubmit": ("", "muninn hook prompt"), "PreToolUse": (pre, "muninn hook pre"),
        "PostToolUse": ("mcp__muninn__.*", "muninn hook post")}
for ev, (matcher, cmd) in want.items():
    lst = [g for g in hooks.get(ev, []) if not any("muninn hook" in h.get("command", "") for h in g.get("hooks", []))]
    g = {"hooks": [{"type": "command", "command": cmd, "timeout": 10}]}
    if matcher: g["matcher"] = matcher
    hooks[ev] = lst + [g]
os.makedirs(os.path.dirname(p), exist_ok=True)
json.dump(s, open(p, "w"), indent=2)
PY
}

if command -v claude >/dev/null; then
  claude mcp remove -s user muninn >/dev/null 2>&1 || true
  claude mcp add-json -s user muninn "{\"type\":\"http\",\"url\":\"$host:7411/mcp\",\"headersHelper\":\"muninn mcp-header\"}"
  install_hooks ~/.claude/settings.json 'Bash|Grep|Glob'
  echo "Claude Code: MCP server 'muninn' and hooks installed"
fi

if command -v codex >/dev/null; then
  mkdir -p ~/.codex
  touch ~/.codex/config.toml
  if ! grep -q '^\[mcp_servers.muninn\]' ~/.codex/config.toml; then
    printf '\n[mcp_servers.muninn]\nurl = "%s:7411/mcp"\nbearer_token_env_var = "MUNINN_TOKEN"\n' "$host" >> ~/.codex/config.toml
  fi
  # Codex names its shell tools shell, local_shell, exec_command or shell_command.
  install_hooks ~/.codex/hooks.json 'Bash|Grep|Glob|shell|local_shell|exec_command|shell_command'
  echo "Codex: MCP server 'muninn' and hooks (~/.codex/hooks.json) installed; export MUNINN_TOKEN=\$(cat ~/.config/muninn/token) in your shell profile"
fi

for d in ~/.claude/skills ~/.codex/skills; do
  [[ -d $(dirname "$d") ]] || continue
  muninn skill --install "$d" >/dev/null
done
muninn doctor
