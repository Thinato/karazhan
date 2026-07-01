# karazhan 🏰

**A terminal cockpit for running coding agents across many git worktrees at once.**

Karazhan turns a wall of git worktrees into a color-coded grid — one square per
worktree / open PR — and lets you fire prompts at them, watch agent sessions run
in the background, and keep tabs on PR and CI status without ever staring at a
raw transcript.

## ✨ Highlights

- 📇 **Prompt library** — reusable prompts as flat Markdown + TOML frontmatter, tracked in git.
- 🟩 **Worktree grid** — one square per `git worktree`, color-coded by status (idle, running, needs review, CI failing, PR merged, error).
- 🧑‍✈️ **Background daemon** — a supervisor owns the agent sessions and filesystem watcher; the TUI can quit and reattach while agents keep running.
- 🔌 **Pluggable agent backend** — Claude Code headless CLI by default; automatic offline **Mock** fallback when `claude` isn't on `PATH`.
- 🐙 **GitHub integration** — polls `gh` for PR state and CI; built-in commands compose context-rich prompts from open review comments or failing CI logs.
- 📂 **Multi-project** — register several repos and see all their worktrees in one grid, grouped by project.
- ⌨️ **Vim-style navigation** + a `Ctrl-p` command palette for everything.

## 📖 Overview

Karazhan (`kah-rah-zhan`) is a Rust TUI for developers who run **many** coding
agents in parallel. Instead of juggling terminal tabs, you keep a library of
prompts and a grid of worktrees. Pick a prompt, spawn a worktree, and the agent
runs headless in the background. You only ever see a coarse per-square status and
a short summary in the detail pane — never the noisy transcript.

**How it works.** A background **daemon** (the supervisor) owns every agent
session and a filesystem watcher; the **TUI client** talks to it over a local
unix socket. Because the daemon is a separate process, you can close the UI and
your agents keep going — reopen karazhan later to reattach. GitHub state (PR
status, CI checks, review comments) is polled through the `gh` CLI, and
context-rich follow-up prompts (address review comments, fix failing CI) are
composed for you.

Built in Rust with [ratatui](https://ratatui.rs) + [crossterm](https://github.com/crossterm-rs/crossterm) for the UI and [tokio](https://tokio.rs) for the async daemon.

## 🚀 Usage

Run karazhan from **inside a git repository** — it uses the current directory as
the initial project root:

```bash
cd /path/to/your/project
karazhan
```

You land in one of two views, toggled with `Tab`:

- **Library** — browse and manage your prompts. Hit `Enter` on a prompt to spawn a worktree that runs it.
- **Grid** — the wall of worktrees. Navigate with `hjkl`, fire commands at the selection, watch status change live.

Press `?` any time for the in-app keybinding overlay, or `Ctrl-p` for the command palette.

### Key bindings

**Global**

| Key | Action |
|---|---|
| `Tab` | Switch between Library and Grid |
| `Ctrl-p` | Open command palette |
| `?` | Toggle help overlay |
| `A` | Add a project (register another git repo) |
| `q` / `Ctrl-C` | Quit the TUI (daemon keeps running) |
| `Q` | Stop the daemon, then quit |

**Library view**

| Key | Action |
|---|---|
| `j` / `k` | Move selection down / up |
| `/` | Filter mode (type to search) |
| `Enter` | New worktree from selected prompt |
| `n` / `a` | Create a new prompt |
| `e` | Edit selected prompt in `$EDITOR` |
| `r` | Reload prompts from disk (pick up new ones) |

**Grid view**

| Key | Action |
|---|---|
| `h` / `j` / `k` / `l` | Move selection (arrows work too) |
| `g` / `G` | Jump to first / last worktree |
| `<n>G` | Jump to worktree at index `n` (e.g. `3G`) |
| `c` | Run a custom free-text prompt on the selection |
| `p` | Address all open PR review comments |
| `i` | Check CI for failures and address them |
| `a` | Toggle auto-continue on PR merge |
| `n` / `N` | New worktree / rename worktree |
| `d` | Delete worktree (asks `y/N`) |
| `o` / `y` / `Y` | Open PR in browser / copy PR URL / copy URL + title |
| `R` | Resume session (recover an errored/interrupted run) |
| `s` | Copy a `cd <worktree> && resume` shell command to debug it yourself |
| `<` / `>` | Widen / narrow the detail pane by 5 cols (`Ctrl` for 1) |
| `r` | Refresh the worktree list |

### Worktree status colors

| Status | Meaning |
|---|---|
| **idle** (dark gray) | No agent session active |
| **running** (yellow) | Agent session in progress |
| **needs review** (magenta) | Agent finished; awaiting human review |
| **CI failing** (red) | CI checks failing for this worktree's PR |
| **PR merged** (green) | Pull request merged |
| **error** (red) | Agent session or command failed |

Colors are configurable — see [Configuration](#-configuration).

## 📦 Installation

**Requirements**

| Dependency | Purpose |
|---|---|
| Rust (stable) | Build the binary |
| [`claude`](https://docs.claude.com/en/docs/claude-code) | Claude Code CLI — the agent backend |
| [`gh`](https://cli.github.com) | GitHub CLI — PR state, CI status, review comments |

If `claude` is missing at startup, karazhan falls back to the offline **Mock**
backend (shown in the status line). If `gh` is missing, GitHub polling and the
built-in PR/CI commands are disabled.

**Build and install:**

```bash
git clone https://github.com/Thinato/karazhan
cd karazhan
cargo install --path .
```

Or run without installing:

```bash
cargo run --manifest-path /path/to/karazhan/Cargo.toml
```

## ⚙️ Configuration

Config is resolved from the first path that exists:

1. `$XDG_CONFIG_HOME/karazhan/config.toml`
2. `~/.config/karazhan/config.toml`

A missing or malformed file is ignored — defaults apply.

```toml
# How often (seconds) to poll GitHub for PR/CI status changes.
poll_interval_secs = 30

# Directory to load prompt files from. Default: <cwd>/prompts
# prompt_dir = "/home/user/my-prompts"

# Binary names (or absolute paths) for the Claude Code and gh CLIs.
claude_bin = "claude"
gh_bin     = "gh"

# Prompt sent to the agent when auto-continue fires after a PR merge.
auto_continue_prompt = "The PR for this worktree was merged. Continue with the next step of the task."

# Per-status color overrides. Supported names: black, red, green, yellow,
# blue, magenta, cyan, gray, dark_gray, white, light_red, light_green,
# light_yellow, light_blue, light_magenta, light_cyan.
[colors]
idle         = "dark_gray"
running      = "yellow"
needs_review = "magenta"
ci_failing   = "red"
pr_merged    = "green"
error        = "red"
```

### Prompt file format

Prompts are individual Markdown files (`<slug>.md`) in the prompt directory,
each with a TOML frontmatter block delimited by `+++`:

```markdown
+++
title = "Address PR comments"
tags  = ["review", "github"]
vars  = []
+++

Read all open review comments on the current PR and address each one.
Commit the changes with a clear message referencing the comment thread.
```

- `title` (string, required) — name shown in the library.
- `tags` (string array, optional) — for filtering.
- `vars` (string array, optional) — variable placeholders (future use).
- Everything after the closing `+++` is the prompt body sent to the agent.

### State & logs

Karazhan persists per-worktree metadata (PR number, status, auto-continue flag)
in `.karazhan/state.toml` inside your repo — safe to commit. Logs roll daily
into `.karazhan/` — add that path to `.gitignore`.

## 🛠️ Development

```bash
cargo test                                  # run the test suite
cargo clippy --all-targets -- -D warnings   # lint
cargo fmt --check                           # format check
```

Tests use a mock `AgentBackend` and a mock `gh` runner — no real `claude` or
`gh` calls are made, so the suite runs offline and in CI.

## 🤝 Contributing & Feedback

Issues, ideas, and PRs are welcome via the
[issue tracker](https://github.com/Thinato/karazhan/issues). If something is
confusing or missing, that's a documentation bug worth reporting too.
