#!/usr/bin/env python3
"""kaeru-first — a harness hook for the moment an agent is about to ask the human.

Usage audit 4 (#89) found that the agent reads kaeru almost always, and almost
never at the moment it asks: 80% of its questions to the user came with no
kaeru read in the previous ten minutes, the median gap being 74 minutes. The
read happens at session entry, then an hour of work, then a question memory
could sometimes have answered. The daemon cannot see that moment. The harness
can — so this hook serves it.

One script, both harnesses. Claude Code and Codex share the hook contract this
relies on: the same event names, the same stdin fields, the same JSON out, and
MCP tools named ``mcp__<server>__<tool>`` in both.

Events, and what each does:

  PostToolUse   on a kaeru READ verb: remember when memory was last consulted.
  PreToolUse    on AskUserQuestion (Claude Code only — Codex asks in plain
                text): if memory was not read inside the window, deny once per
                turn and hand back the search recipe.
  Stop          if the reply ends in a question and memory was not read inside
                the window, block once and hand back the recipe. Always records
                whether the reply was a question, for the next event.
  UserPromptSubmit
                if the previous reply was a question and this answer is
                substantial, remind the agent that the answer was not in kaeru
                and should be captured.

It never parses a transcript. The transcript format is an internal detail of
each harness — Codex's has visibly changed across versions — and Claude Code's
documentation warns that the file can lag the live conversation, which would
make a search that just happened invisible and fire the hook for nothing. The
documented hook fields are the contract; a small per-session state file fed by
``PostToolUse`` replaces everything a transcript read was for.

It fails open. Any error — unreadable input, an unwritable state directory, an
event it does not know — exits 0 with no output. A hook that breaks the
harness is worse than no hook.

Environment:

  KAERU_FIRST_WINDOW     seconds a kaeru read stays "recent" (default 600 —
                         a first guess from the audit's 74-minute median).
  KAERU_FIRST_STATE_DIR  where per-session state lives (default
                         $XDG_STATE_HOME/kaeru-first, else
                         ~/.local/state/kaeru-first).
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
from pathlib import Path

# kaeru verbs that CONSULT memory. A read is what the hook is looking for, so a
# write (`jot`, `link`, …) never counts.
#
# `initiatives` is left out on purpose: it lists scopes, it does not read what
# is in them. The audit's clearest miss was a 17-item questionnaire asked of
# the user right after a single `initiatives` call — counting that as a read
# would have waved it through.
#
# An allowlist rather than a denylist so that the failure mode of a new verb
# is an extra nudge (one needless search) rather than a missed one. Revisit
# when the verb surface is re-tiered (#79).
READ_VERBS = frozenset(
    {
        "at",
        "awake",
        "between",
        "board",
        "board_status",
        "cloud_initiatives",
        "cloud_links",
        "cloud_recall",
        "drill",
        "history",
        "ideas",
        "lint",
        "neighbours",
        "outcomes",
        "overview",
        "path",
        "recall",
        "recent",
        "reflect",
        "search",
        "slots",
        "surface",
        "tagged",
        "trace",
        "why",
    }
)

KAERU_TOOL = re.compile(r"^mcp__(?:plugin_[^_]+_)?kaeru__(?P<verb>[a-z_]+)$")

# A reply this short is an acknowledgement, not knowledge worth capturing.
SUBSTANTIAL_ANSWER = 30

STATE_TTL = 7 * 24 * 3600

RECIPE = (
    "Before you ask the user, check kaeru — the answer may already be in memory.\n"
    "1. `search` WITHOUT `initiative`: initiatives fragment, and a scoped search "
    "misses a node that lives in a sibling scope.\n"
    "2. Put a prefix wildcard on each entity in your question (`certif*`, "
    "`deploy*`), and search in the language the notes were written in.\n"
    "3. If the question is \"what next\", that is `awake` + `board`, not a "
    "question for the user.\n"
    "4. `at <name>` on any hit to read it in full.\n"
    "If nothing turns up, ask — and say in one line what you searched, so the "
    "user knows memory was tried first."
)

CAPTURE = (
    "The user just answered a question you asked. By construction that answer "
    "was not in kaeru — capture it now: `episode` (or `cite` for a settled "
    "fact) with the `initiative`, and `link` it to what it is about. Rules "
    "spoken in the dialogue and never captured are the most frequent class of "
    "interruption in the usage audits."
)


# ---------------------------------------------------------------- state -----


def state_dir() -> Path:
    explicit = os.environ.get("KAERU_FIRST_STATE_DIR")
    if explicit:
        return Path(explicit)
    base = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
    return Path(base) / "kaeru-first"


def state_path(session_id: str) -> Path:
    # A session id becomes a file name — keep it to a safe alphabet.
    safe = re.sub(r"[^A-Za-z0-9_.-]", "_", session_id) or "unknown"
    return state_dir() / f"{safe}.json"


def load_state(session_id: str) -> dict:
    try:
        return json.loads(state_path(session_id).read_text())
    except (OSError, ValueError):
        return {}


def save_state(session_id: str, state: dict) -> None:
    directory = state_dir()
    directory.mkdir(parents=True, exist_ok=True)
    state["updated"] = time.time()
    path = state_path(session_id)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(state))
    tmp.replace(path)
    prune(directory)


def prune(directory: Path) -> None:
    """Drop state for sessions untouched for a week. Best-effort."""
    cutoff = time.time() - STATE_TTL
    try:
        for f in directory.glob("*.json"):
            if f.stat().st_mtime < cutoff:
                f.unlink(missing_ok=True)
    except OSError:
        pass


# ------------------------------------------------------------- helpers ------


def window() -> float:
    try:
        return float(os.environ.get("KAERU_FIRST_WINDOW", "600"))
    except ValueError:
        return 600.0


def read_recently(state: dict) -> bool:
    last = state.get("last_read")
    return isinstance(last, (int, float)) and time.time() - last <= window()


def turn_key(event: dict) -> str:
    # Claude Code names a turn `prompt_id`, Codex `turn_id`.
    return str(event.get("turn_id") or event.get("prompt_id") or "")


def ends_in_question(text: str | None) -> bool:
    """Whether the last non-empty line of a reply is a question.

    Only the last line counts: a rhetorical question mid-message is not the
    agent asking the user anything.
    """
    if not text:
        return False
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    if not lines:
        return False
    last = lines[-1].rstrip("*_`)\"'»”’ ")
    return last.endswith(("?", "？"))


def deny(reason: str) -> dict:
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }


# -------------------------------------------------------------- events ------


def on_post_tool_use(event: dict, state: dict) -> dict | None:
    match = KAERU_TOOL.match(event.get("tool_name") or "")
    if match and match.group("verb") in READ_VERBS:
        state["last_read"] = time.time()
    return None


def on_pre_tool_use(event: dict, state: dict) -> dict | None:
    if event.get("tool_name") != "AskUserQuestion":
        return None
    if read_recently(state):
        return None
    # Once per turn. The agent may have searched and still need to ask; a
    # second denial in the same turn would only be an obstacle.
    turn = turn_key(event)
    denied = state.setdefault("denied_turns", [])
    if turn and turn in denied:
        return None
    denied.append(turn)
    del denied[:-20]
    return deny(RECIPE)


def on_stop(event: dict, state: dict) -> dict | None:
    question = ends_in_question(event.get("last_assistant_message"))
    # Recorded on every Stop, including a continued one, so the next prompt
    # knows whether it is answering a question.
    state["last_reply_question"] = question
    # Both harnesses set this once a Stop hook has already continued the turn.
    # Honouring it is the loop guard: block once, never twice.
    if event.get("stop_hook_active"):
        return None
    if not question or read_recently(state):
        return None
    return {"decision": "block", "reason": RECIPE}


def on_user_prompt_submit(event: dict, state: dict) -> dict | None:
    was_question = bool(state.get("last_reply_question"))
    state["last_reply_question"] = False
    prompt = (event.get("prompt") or "").strip()
    if not was_question or len(prompt) < SUBSTANTIAL_ANSWER:
        return None
    return {
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": CAPTURE,
        }
    }


HANDLERS = {
    "PostToolUse": on_post_tool_use,
    "PreToolUse": on_pre_tool_use,
    "Stop": on_stop,
    "UserPromptSubmit": on_user_prompt_submit,
}


def main() -> int:
    try:
        event = json.loads(sys.stdin.read() or "{}")
        handler = HANDLERS.get(event.get("hook_event_name", ""))
        session = event.get("session_id")
        if handler is None or not session:
            return 0
        state = load_state(session)
        out = handler(event, state)
        save_state(session, state)
        if out is not None:
            sys.stdout.write(json.dumps(out))
    except Exception as exc:  # noqa: BLE001 — fail open, always
        print(f"kaeru-first: {exc}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
