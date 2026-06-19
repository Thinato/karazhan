# karazhan

A terminal UI for managing a library of prompts and driving coding agents
(Claude Code) across git worktrees.

## Concept

Karazhan gives you a **grid of squares** — one per git worktree / open PR.
You navigate with vim keys, fire prompts at worktrees, and watch agent
sessions run in the background.  You never see the raw agent transcript;
only a coarse status per square (idle, running, needs review, CI failing, PR
merged, error) and a short summary line in the detail pane.

Key ideas:

- **Prompt library** — flat Markdown files with TOML frontmatter, stored in a
  directory you control and tracked in git.
- **Worktree grid** — one square per `git worktree`, color-coded by status.
- **Pluggable agent backend** — Claude Code headless CLI by default; falls
  back to an offline mock when `claude` is not on PATH.
- **GitHub integration** — polls `gh` for PR state and CI runs.  Built-in
  commands compose context-rich prompts from open review comments or failing
  CI logs.
- **Auto-continue on merge** — when a PR merges the agent can automatically
  continue the session; toggle per worktree.

---

## Requirements

| Dependency | Purpose |
|---|---|
| `claude` | Claude Code CLI — agent backend |
| `gh` | GitHub CLI — PR state, CI status, review comments |
| Rust stable | Build tool |

If `claude` is absent at startup, karazhan falls back to the offline **Mock**
backend (shown in the status line).  If `gh` is absent, background polling is
disabled and built-in PR/CI commands are unavailable.

---

## Install & Run

```bash
# Clone the repo and build
git clone https://github.com/Thinato/karazhan
cd karazhan
cargo build --release

# Run from inside a git repository
cd /path/to/your/project
karazhan          # or: cargo run --manifest-path /path/to/karazhan/Cargo.toml
```

karazhan must be run from inside a git repository — it uses the current
directory as the repo root for `git worktree list` and `gh` calls.

---

## Configuration

Config file location (resolved in order):

1. `$XDG_CONFIG_HOME/karazhan/config.toml`
2. `~/.config/karazhan/config.toml`

A missing or malformed file is silently ignored — defaults are used.

### Example `config.toml`

```toml
# How often (seconds) to poll GitHub for PR/CI status changes.
poll_interval_secs = 30

# Directory to load prompt files from.
# Default: <cwd>/prompts
# prompt_dir = "/home/user/my-prompts"

# Binary names (or absolute paths) for the Claude Code and gh CLIs.
claude_bin = "claude"
gh_bin     = "gh"

# Prompt sent to the agent when auto-continue fires after a PR merge.
auto_continue_prompt = "The PR for this worktree was merged. Continue with the next step of the task."

# Per-status colour overrides.
# Supported colour names: black, red, green, yellow, blue, magenta, cyan,
# gray, dark_gray, white, light_red, light_green, light_yellow, light_blue,
# light_magenta, light_cyan.
[colors]
idle         = "dark_gray"
running      = "yellow"
needs_review = "magenta"
ci_failing   = "red"
pr_merged    = "green"
error        = "red"
```

---

## Prompt File Format

Prompts live as individual Markdown files in the prompt directory
(`<cwd>/prompts` by default).  Each file is named `<slug>.md`.

```
+++
title = "Address PR comments"
tags  = ["review", "github"]
vars  = []
+++

Read all open review comments on the current PR and address each one.
Commit the changes with a clear message referencing the comment thread.
```

- The frontmatter block is delimited by `+++` lines and contains TOML.
- `title` (string, required) — human-readable name shown in the library.
- `tags` (string array, optional) — for filtering.
- `vars` (string array, optional) — variable placeholders (future use).
- Everything after the closing `+++` is the prompt body sent to the agent.

---

## Keybindings

### Global

| Key | Action |
|---|---|
| `Tab` | Switch between Library and Grid view |
| `?` | Toggle help overlay |
| `q` | Quit |
| `Ctrl-C` | Quit (always works) |

### Library view

| Key | Action |
|---|---|
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `/` | Enter filter mode (type to search) |
| `n` / `a` | Create a new prompt |
| `Esc` | Clear filter / cancel input |
| `Enter` | Confirm new prompt title |
| `Backspace` | Delete last character in input |

### Grid view (Normal mode)

| Key | Action |
|---|---|
| `h` / `←` | Move selection left |
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `l` / `→` | Move selection right |
| `g` | Jump to first worktree |
| `G` | Jump to last worktree |
| `<n>G` | Jump to worktree at index `n` (e.g. `3G`) |
| `c` | Run a custom free-text prompt on the selected worktree |
| `p` | Address all open PR review comments |
| `i` | Check CI for failures and address them |
| `a` | Toggle auto-continue-on-merge for the selected worktree |
| `r` | Refresh the worktree list |
| `q` | Quit |

### Grid view (Prompt input mode — entered via `c`)

| Key | Action |
|---|---|
| `Enter` | Send the prompt to the agent |
| `Esc` | Cancel prompt input |
| `Backspace` | Delete last character |

---

## Worktree Status Colors

| Status | Meaning |
|---|---|
| **idle** (dark gray) | No agent session active |
| **running** (yellow) | Agent session in progress |
| **needs review** (magenta) | Agent finished; awaiting human review |
| **CI failing** (red) | CI checks are failing for this worktree's PR |
| **PR merged** (green) | Pull request was merged |
| **error** (red) | Agent session or command failed |

Colors are configurable via `[colors]` in `config.toml`.

---

## Agent Backend Pluggability

The active backend is shown in the status line at the bottom of the detail pane.

- **ClaudeCode** — real `claude` CLI on PATH.  Spawns `claude -p <prompt> --output-format stream-json --verbose` in the worktree directory.  Sessions are hidden — only coarse status reaches the UI.
- **Mock** — offline fallback when `claude` is not found.  Simulates a session with fixed status progression; useful for development without a real Claude subscription.

To use a custom binary name or path, set `claude_bin` in `config.toml`.

---

## State File

karazhan persists worktree metadata (PR number, status, auto-continue flag) in
`.karazhan/state.toml` inside your repository.  This file is safe to commit.
Logs are written to `.karazhan/karazhan.log` (daily rolling) and should be
added to `.gitignore`.

---

## Development

```bash
cargo test                              # run all tests (~77)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests use a mock `AgentBackend` and mock `GhRunner` — no real `claude` or
`gh` calls are made in CI.
