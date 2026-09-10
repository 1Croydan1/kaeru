"""Scenario tests for kaeru-first. Stdlib only: `python3 -m unittest -v`.

Each test runs the hook exactly as a harness does — a subprocess fed one JSON
event on stdin — against its own scratch state directory.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HOOK = Path(__file__).with_name("kaeru_first.py")


class HookHarness(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        self.session = "sess-1"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def fire(self, event: dict, **env: str) -> dict | None:
        event = {"session_id": self.session, **event}
        proc = subprocess.run(
            [sys.executable, str(HOOK)],
            input=json.dumps(event),
            capture_output=True,
            text=True,
            env={**os.environ, "KAERU_FIRST_STATE_DIR": self.state_dir, **env},
            timeout=10,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return json.loads(proc.stdout) if proc.stdout.strip() else None

    def read_kaeru(self, verb: str = "search") -> None:
        self.fire(
            {"hook_event_name": "PostToolUse", "tool_name": f"mcp__kaeru__{verb}"}
        )

    def ask(self, turn: str = "t1") -> dict | None:
        return self.fire(
            {
                "hook_event_name": "PreToolUse",
                "tool_name": "AskUserQuestion",
                "prompt_id": turn,
            }
        )

    def stop(self, message: str, active: bool = False) -> dict | None:
        return self.fire(
            {
                "hook_event_name": "Stop",
                "last_assistant_message": message,
                "stop_hook_active": active,
            }
        )

    def age_last_read(self, seconds: float) -> None:
        path = Path(self.state_dir) / f"{self.session}.json"
        state = json.loads(path.read_text())
        state["last_read"] = time.time() - seconds
        path.write_text(json.dumps(state))


class AskUserQuestionTests(HookHarness):
    def test_asking_without_reading_memory_is_denied_with_the_recipe(self):
        out = self.ask()
        decision = out["hookSpecificOutput"]
        self.assertEqual(decision["permissionDecision"], "deny")
        self.assertIn("WITHOUT `initiative`", decision["permissionDecisionReason"])

    def test_asking_right_after_a_search_passes(self):
        self.read_kaeru("search")
        self.assertIsNone(self.ask())

    def test_a_denial_happens_once_per_turn(self):
        self.assertIsNotNone(self.ask("t1"))
        self.assertIsNone(self.ask("t1"), "the agent may still need to ask")
        self.assertIsNotNone(self.ask("t2"), "a new turn starts fresh")

    def test_other_tools_are_ignored(self):
        out = self.fire({"hook_event_name": "PreToolUse", "tool_name": "Bash"})
        self.assertIsNone(out)

    def test_a_stale_read_does_not_count(self):
        self.read_kaeru("search")
        self.age_last_read(3600)
        self.assertIsNotNone(self.ask(), "an hour-old read is not 'recent'")

    def test_the_window_is_configurable(self):
        self.read_kaeru("search")
        self.age_last_read(120)
        self.assertIsNotNone(self.fire(
            {"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
             "prompt_id": "t9"},
            KAERU_FIRST_WINDOW="60",
        ))


class WhatCountsAsAReadTests(HookHarness):
    def test_initiatives_alone_is_not_a_read(self):
        # The audit's 17-item questionnaire came right after one `initiatives`.
        self.read_kaeru("initiatives")
        self.assertIsNotNone(self.ask())

    def test_a_write_is_not_a_read(self):
        self.read_kaeru("jot")
        self.assertIsNotNone(self.ask())

    def test_awake_is_a_read(self):
        self.read_kaeru("awake")
        self.assertIsNone(self.ask())

    def test_other_servers_do_not_count(self):
        self.fire({"hook_event_name": "PostToolUse", "tool_name": "mcp__github__search"})
        self.assertIsNotNone(self.ask())

    def test_a_plugin_bundled_kaeru_counts(self):
        self.fire({"hook_event_name": "PostToolUse",
                   "tool_name": "mcp__plugin_lamantin_kaeru__search"})
        self.assertIsNone(self.ask())


class StopTests(HookHarness):
    def test_a_reply_ending_in_a_question_is_blocked_once(self):
        out = self.stop("I checked the config.\n\nShould I rotate the certificate?")
        self.assertEqual(out["decision"], "block")
        self.assertIn("check kaeru", out["reason"])

    def test_the_loop_guard_is_honoured(self):
        self.assertIsNone(self.stop("Shall I continue?", active=True))

    def test_a_question_after_a_read_passes(self):
        self.read_kaeru("at")
        self.assertIsNone(self.stop("Which endpoint do we keep?"))

    def test_a_statement_passes(self):
        self.assertIsNone(self.stop("Done — all 352 tests are green."))

    def test_only_the_last_line_counts(self):
        self.assertIsNone(self.stop("Why did it fail? A stale lock.\nFixed it."))

    def test_markdown_around_the_question_mark_is_seen_through(self):
        self.assertIsNotNone(self.stop("**Deploy to prod now?**"))

    def test_a_full_width_question_mark_counts(self):
        self.assertIsNotNone(self.stop("どうしますか？"))


class CaptureTheAnswerTests(HookHarness):
    def prompt(self, text: str) -> dict | None:
        return self.fire({"hook_event_name": "UserPromptSubmit", "prompt": text})

    def test_a_substantial_answer_to_a_question_asks_for_capture(self):
        self.read_kaeru("search")  # so the Stop passes and only records
        self.stop("Where does the admin panel live?")
        out = self.prompt("It is at admin.internal behind the VPN, port 8443, ask ops.")
        self.assertIn("capture it", out["hookSpecificOutput"]["additionalContext"])

    def test_a_short_acknowledgement_does_not(self):
        self.read_kaeru("search")
        self.stop("Proceed?")
        self.assertIsNone(self.prompt("yes"))

    def test_a_prompt_not_answering_a_question_does_not(self):
        self.stop("Done.")
        self.assertIsNone(self.prompt("Now let us move on to the next issue, please."))

    def test_the_reminder_fires_once_not_for_every_later_prompt(self):
        self.read_kaeru("search")
        self.stop("Which region?")
        self.assertIsNotNone(self.prompt("eu-central, same as the staging stand we set up."))
        self.assertIsNone(self.prompt("and another long message that is not an answer"))


class CodexShapeTests(HookHarness):
    def test_codex_turn_ids_drive_the_once_per_turn_guard(self):
        ev = {"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
              "turn_id": "codex-turn-1"}
        self.assertIsNotNone(self.fire(ev))
        self.assertIsNone(self.fire(ev))

    def test_codex_stop_blocks_the_same_way(self):
        out = self.fire({"hook_event_name": "Stop", "turn_id": "x",
                         "last_assistant_message": "Want me to open the PR?",
                         "stop_hook_active": False})
        self.assertEqual(out, {"decision": "block", "reason": out["reason"]})


class FailOpenTests(HookHarness):
    def run_raw(self, stdin: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(HOOK)], input=stdin, capture_output=True, text=True,
            env={**os.environ, "KAERU_FIRST_STATE_DIR": self.state_dir}, timeout=10,
        )

    def test_garbage_input_exits_zero_silently(self):
        proc = self.run_raw("not json at all")
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout, "")

    def test_an_unknown_event_is_ignored(self):
        self.assertIsNone(self.fire({"hook_event_name": "PreCompact"}))

    def test_an_unwritable_state_dir_fails_open(self):
        proc = subprocess.run(
            [sys.executable, str(HOOK)],
            input=json.dumps({"session_id": "s", "hook_event_name": "Stop",
                              "last_assistant_message": "ok?"}),
            capture_output=True, text=True, timeout=10,
            env={**os.environ, "KAERU_FIRST_STATE_DIR": "/proc/definitely/not/writable"},
        )
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout, "", "no half-made decision")


if __name__ == "__main__":
    unittest.main()
