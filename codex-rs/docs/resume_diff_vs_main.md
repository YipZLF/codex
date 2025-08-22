# Resume Feature: Diff vs main

This document captures the changes on branch `feature/resume` compared to `main`.

## Summary

- TUI: Added resume timeline picker (newest-first, fast navigation, cwd hints), removed /branch UI, and clears viewport on resume.
- Core: Added `previous_response_id` support; record state snapshots with `last_response_id`, `created_at`, and `summary`; fixed persistence so internal context (`<user_instructions>`, `<environment_context>`) is kept only in in-memory history (not in rollout).
- CLI: `sessions` subcommands (list/show/resume) with `--at` and `--step` options.
- Docs/Help: Updated `/resume` description and added `codex_resume.md`.
- Bug fix: UTF-8 safe truncation for synthesized summaries to avoid char boundary panics.

## Diff Stat (main..feature/resume)

```
 codex-rs/Cargo.lock                                |   2 +
 codex-rs/README.md                                 |  15 +
 codex-rs/cli/Cargo.toml                            |   2 +
 codex-rs/cli/src/main.rs                           |  10 +
 codex-rs/cli/src/sessions.rs                       | 393 +++++++++++++++++++++
 codex-rs/core/src/client.rs                        |   5 +
 codex-rs/core/src/client_common.rs                 |   7 +
 codex-rs/core/src/codex.rs                         | 139 +++++++-
 codex-rs/core/src/config.rs                        |  11 +
 codex-rs/core/src/rollout.rs                       |   9 +-
 codex-rs/docs/codex_resume.md                      |  66 ++++
 codex-rs/exec/src/lib.rs                           |   1 +
 codex-rs/tui/src/app.rs                            | 170 +++++++++
 codex-rs/tui/src/app_event.rs                      |   8 +
 codex-rs/tui/src/bottom_pane/chat_composer.rs      |  33 +-
 .../tui/src/bottom_pane/list_selection_view.rs     |  42 +++
 codex-rs/tui/src/chatwidget.rs                     | 363 ++++++++++++++++++-
 codex-rs/tui/src/history_cell.rs                   |  14 +
 codex-rs/tui/src/slash_command.rs                  |   2 +
 codex-rs/tui/src/tui.rs                            |   7 +
 docs/codex_resume.md                               | 169 +++++++++
 21 files changed, 1446 insertions(+), 22 deletions(-)
```

## Name-Status (main..feature/resume)

```
 M	codex-rs/Cargo.lock
 M	codex-rs/README.md
 M	codex-rs/cli/Cargo.toml
 M	codex-rs/cli/src/main.rs
 A	codex-rs/cli/src/sessions.rs
 M	codex-rs/core/src/client.rs
 M	codex-rs/core/src/client_common.rs
 M	codex-rs/core/src/codex.rs
 M	codex-rs/core/src/config.rs
 M	codex-rs/core/src/rollout.rs
 A	codex-rs/docs/codex_resume.md
 M	codex-rs/exec/src/lib.rs
 M	codex-rs/tui/src/app.rs
 M	codex-rs/tui/src/app_event.rs
 M	codex-rs/tui/src/bottom_pane/chat_composer.rs
 M	codex-rs/tui/src/bottom_pane/list_selection_view.rs
 M	codex-rs/tui/src/chatwidget.rs
 M	codex-rs/tui/src/history_cell.rs
 M	codex-rs/tui/src/slash_command.rs
 M	codex-rs/tui/src/tui.rs
 A	docs/codex_resume.md
```

## Commit Log (main..feature/resume)

```
 0de0b9ed (HEAD -> feature/resume) chore: remove codex-resume.zip and design-chat.md from repo
 defb81f5 core: do not persist internal context messages (<user_instructions>, <environment_context>) to rollout; keep only in-memory conversation history; TUI: revert tag-based filtering (root-cause fixed in core)
 bc9fa5b0 tui: fix UTF-8 safe truncation in legacy summary synthesis to avoid char boundary panic
 503244e7 (feat/codex-resume-cli) tui: resume timeline UX; docs/help updates; legacy summaries, newest-first, cwd hints; clear viewport on resume; hide internal messages; remove /branch
 d1d9e0c9 docs: add codex_resume.md summarizing design, implementation, usage, progress, and next steps for resume/branch feature
 a77d9cec tui: adjust /resume to not require prompt; replay rollout transcript (user/assistant messages) into UI; add /branch + /resume requests; show composer empty after resume
 1c9d2e13 cli: sessions show now prints time/summary; sessions resume supports --step; core: record created_at/summary in rollout state; tui: add /resume and /branch commands to resume/branch within TUI
 5252ec9c core: add previous_response_id resume support and record last_response_id in rollout state; cli: sessions show/branch/checkout and --at <response_id> to resume via server chaining
 b82c2ac1 cli: add \u001b[?2004h\u001b[>7u\u001b[2J\u001b[1;1H\u001b[6n subcommand with list/resume (MVP) using experimental rollout resume
```

## Notable Code Touchpoints

- core
  - `core/src/codex.rs`: do not persist internal context messages to rollout; keep in-memory history only; filter persisted items when recording state.
  - `core/src/client_common.rs`: prompt structure for instructions and base instructions.
  - `core/src/rollout.rs`: state snapshots (`last_response_id`, `created_at`, `summary`).
- tui
  - `tui/src/chatwidget.rs`: timeline building, sorting, legacy summary synthesis (UTF-8 safe), cwd extraction, removal of /branch parsing.
  - `tui/src/app.rs`: `/resume` opens timeline, clears viewport on resume, replays only user/assistant messages.
  - `tui/src/bottom_pane/list_selection_view.rs`: fast navigation keys.
  - `tui/src/slash_command.rs`: `/resume` help text.
- cli
  - `cli/src/sessions.rs`: list/show/resume with `--at` and `--step`.
- docs
  - `codex-rs/docs/codex_resume.md`: How to use resume/rollback.
