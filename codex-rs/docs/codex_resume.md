# Resume, Rollback, and Branch (Rust TUI/CLI)

This document explains how to resume a session, roll back to an earlier turn,
and branch naturally from past steps in the Rust Codex CLI/TUI.

## Overview

- Resuming restores server-side context using `previous_response_id` when
  possible, and locally replays transcript history for compatibility.
- Rolling back is implicit: resuming from an earlier turn creates a new session
  that branches from that point.
- The TUI provides a timeline to browse all recorded sessions/steps without
  typing parameters.

## TUI Flow (Recommended)

- Run Codex TUI and invoke `/resume` (no arguments). This opens a timeline view.
- Use arrow keys to navigate; fast navigation keys:
  - PageUp/PageDown/Home/End to jump faster.
- Sorting and display:
  - Items are sorted by time (newest first).
  - Each item shows session id, step index, timestamp, a short summary, and (if
    available) a cwd hint derived from previous commands.
  - When resuming sessions that predate state snapshots, Codex synthesizes a
    short summary from the last assistant/user message and uses the file
    modification time as a timestamp.
- Selecting an item clears the current chat area, replays prior user/assistant
  messages for context, and starts a new session that resumes from the selected
  turn.
- Internal system/developer blocks like `<user_instructions>` and
  `<environment_context>` are not shown in the transcript.

## CLI Flow

The multi-tool `codex` CLI offers session utilities:

- `codex sessions list` – list recorded rollout files
- `codex sessions show <SID|PATH>` – show discovered steps and timestamps
- `codex sessions resume <SID|PATH> --prompt "..." [--at RESP_ID | --step N]` –
  resume from a specific response id or n-th recorded step

Notes:

- `--at` leverages server-side chaining with `previous_response_id` to avoid
  resending full history.
- `--step` is a convenience that scans the rollout’s state lines to find the
  matching response id.
- If a rollout lacks state lines (older sessions), the CLI still works and
  resumes with local history replay.

## Compatibility

- New sessions record lightweight state snapshots with `last_response_id`, an
  RFC3339 `created_at`, and a short textual `summary`. These power the timeline.
- Older sessions without snapshots still appear once (synthesized summary +
  timestamp), and the TUI/CLI can resume them correctly.

## Tips

- For users working across multiple repositories, the timeline includes a
  `cwd:` hint when discoverable and prioritizes items matching the current
  working directory.
- Branching is implicit: resuming from an early step naturally creates a new
  session that continues from that point (no dedicated `/branch` command in
  TUI).

