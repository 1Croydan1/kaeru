# kaeru-first — check memory before asking the human

A harness hook for **Claude Code** and **Codex**. It serves one moment: the
agent is about to ask the user something. If memory has not been read
recently, it sends the agent to kaeru first — once — and if the user then
answers, it reminds the agent to capture the answer.

## Why

Usage audit 4 (#89): the agent reads kaeru almost always, just never at the
moment it asks. **80% of its questions to the user came with no kaeru read in
the previous ten minutes**; the median gap was 74 minutes. The read happens at
session entry, then an hour of work, then a question. In eight cases the
user's own reply was some form of *"it's in kaeru"*.

A hint in tool output does not create an intention (#79 — `why` got 0 calls at
7 delivered hints). But this moment is different: the harness sees it
deterministically, so the nudge can be conditional and land exactly when it
matters.

## What it does

| event | condition | action |
|---|---|---|
| `PostToolUse` on a kaeru read verb | — | remembers when memory was last consulted |
| `PreToolUse` on `AskUserQuestion` *(Claude Code)* | no read in the window | denies, once per turn, with the search recipe |
| `Stop` | reply ends in `?`, no read in the window | blocks once, same recipe |
| `UserPromptSubmit` | previous reply was a question, answer ≥ 30 chars | asks the agent to capture the answer |

The recipe: `search` **without** `initiative` (scopes fragment), prefix
wildcards on the entities of the question, `awake` + `board` for "what next",
`at` on hits — and if nothing turns up, ask and say what was searched.

**Counts as a read:** `search`, `at`, `drill`, `awake`, `recall`, `neighbours`,
`why`, `board` and the other verbs that consult memory. **Does not:** writes,
and `initiatives` — it lists scopes without reading them, and the audit's
clearest miss was a 17-item questionnaire right after one `initiatives` call.

Codex has no `AskUserQuestion` — it asks in plain text — so there the `Stop`
row does the work.

## Design notes

- **No transcript parsing.** Both harnesses pass everything needed as
  documented hook fields (`last_assistant_message`, `stop_hook_active`,
  `prompt`, `tool_name`). Transcript formats are internal and differ between
  the two, and Claude Code's docs warn the file can lag the live
  conversation — which would hide a search that just happened.
- **Blocks once, never loops.** `Stop` honours `stop_hook_active`, and the
  `AskUserQuestion` denial is once per turn: an agent that searched and still
  needs to ask, asks.
- **Fails open.** Unreadable input, an unwritable state directory, an unknown
  event: exit 0, no output. A hook must never break the harness.
- State is one small JSON file per session in `$XDG_STATE_HOME/kaeru-first`
  (or `~/.local/state/kaeru-first`), pruned after a week.

## Install

Needs `python3` (stdlib only). Copy the script somewhere stable:

```sh
mkdir -p ~/.local/share/kaeru-first
cp kaeru_first.py ~/.local/share/kaeru-first/
chmod +x ~/.local/share/kaeru-first/kaeru_first.py
```

### Claude Code — `~/.claude/settings.json`

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "mcp__kaeru__.*",
        "hooks": [{ "type": "command", "command": "~/.local/share/kaeru-first/kaeru_first.py", "timeout": 10 }] }
    ],
    "PreToolUse": [
      { "matcher": "AskUserQuestion",
        "hooks": [{ "type": "command", "command": "~/.local/share/kaeru-first/kaeru_first.py", "timeout": 10 }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "~/.local/share/kaeru-first/kaeru_first.py", "timeout": 10 }] }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "~/.local/share/kaeru-first/kaeru_first.py", "timeout": 10 }] }
    ]
  }
}
```

### Codex — `~/.codex/config.toml`

```toml
[[hooks.PostToolUse]]
matcher = "^mcp__kaeru__"
[[hooks.PostToolUse.hooks]]
type = "command"
command = "~/.local/share/kaeru-first/kaeru_first.py"
timeout = 10

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "~/.local/share/kaeru-first/kaeru_first.py"
timeout = 10

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "~/.local/share/kaeru-first/kaeru_first.py"
timeout = 10
```

Hooks in Codex are listed among its feature flags (`codex_hooks`). If the
hooks above never fire on your version, enable them:

```toml
[features]
codex_hooks = true
```

## Tuning

| variable | default | meaning |
|---|---|---|
| `KAERU_FIRST_WINDOW` | `600` | seconds a kaeru read counts as recent — a first guess from the audit's 74-minute median |
| `KAERU_FIRST_STATE_DIR` | `$XDG_STATE_HOME/kaeru-first` | where per-session state lives |

## Tests

```sh
python3 -m unittest -v
```

Each scenario runs the script as a harness does — a subprocess fed one JSON
event — against a scratch state directory.
