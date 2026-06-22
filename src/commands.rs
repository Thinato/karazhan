//! Central command registry and command-palette logic.
//!
//! This module is the single source of truth for every *actionable* command in
//! the TUI.  Each command has exactly one [`CommandId`] variant, one
//! [`CommandSpec`] entry in [`ALL_COMMANDS`], and one match arm in
//! `App::execute_command` (in `app.rs`).
//!
//! Enforcement of "every command must be listed in the palette":
//!   * `App::execute_command` matches on `CommandId` *exhaustively* (no
//!     wildcard arm), so adding a new variant without a handler is a **compile
//!     error**.
//!   * The `registry_lists_every_command_once` unit test asserts that every
//!     `CommandId` appears in `ALL_COMMANDS` exactly once (no missing, no
//!     duplicates) — a runtime guard against forgetting the registry entry.
//!
//! Pure cursor-movement keys (h/j/k/l, g/G, digit counts, list move) are
//! deliberately *not* commands — they stay inline in the per-view key handlers.

// ---------------------------------------------------------------------------
// CommandId
// ---------------------------------------------------------------------------

/// One variant per actionable command.  Cursor movement is excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandId {
    /// Toggle between the Library and Grid views (same as `Tab`).
    SwitchView,
    /// Toggle the help overlay (same as `?`).
    ToggleHelp,
    /// Quit the TUI; the daemon keeps running (same as `q`).
    Quit,
    /// Stop the supervisor daemon, then quit (same as `Q`).
    StopDaemon,
    /// Create a new prompt (Library; same as `n`/`a`).
    NewPrompt,
    /// Edit the selected prompt in `$EDITOR` (Library; same as `e`).
    EditPrompt,
    /// Enter prompt-library filter mode (Library; same as `/`).
    FilterPrompts,
    /// Re-scan worktrees (Grid; same as `r`).
    RefreshWorktrees,
    /// Run a custom free-text prompt on the selected worktree (Grid; same as `c`).
    RunCustomPrompt,
    /// Address all open PR review comments on the selection (Grid; same as `p`).
    AddressPrComments,
    /// Check CI failures on the selection (Grid; same as `i`).
    CheckCi,
    /// Toggle auto-continue on PR merge for the selection (Grid; same as `a`).
    ToggleAutoContinue,
}

// ---------------------------------------------------------------------------
// CommandContext
// ---------------------------------------------------------------------------

/// Which view(s) a command applies to.  `Global` commands are available from
/// both views; `Library`/`Grid` commands only appear in the palette when it is
/// opened from that view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandContext {
    Global,
    Library,
    Grid,
}

// ---------------------------------------------------------------------------
// CommandSpec
// ---------------------------------------------------------------------------

/// Human-facing metadata for a single command, shown in the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub id: CommandId,
    pub title: &'static str,
    pub description: &'static str,
    pub keybind: &'static str,
    pub context: CommandContext,
}

/// The registry: every [`CommandId`] must appear here exactly once.
pub const ALL_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::SwitchView,
        title: "Switch View",
        description: "Toggle between Library and Grid",
        keybind: "Tab",
        context: CommandContext::Global,
    },
    CommandSpec {
        id: CommandId::ToggleHelp,
        title: "Toggle Help",
        description: "Show or hide the help overlay",
        keybind: "?",
        context: CommandContext::Global,
    },
    CommandSpec {
        id: CommandId::Quit,
        title: "Quit",
        description: "Quit the TUI (daemon keeps running)",
        keybind: "q",
        context: CommandContext::Global,
    },
    CommandSpec {
        id: CommandId::StopDaemon,
        title: "Stop Daemon and Quit",
        description: "Stop the supervisor daemon, then quit",
        keybind: "Q",
        context: CommandContext::Global,
    },
    CommandSpec {
        id: CommandId::NewPrompt,
        title: "New Prompt",
        description: "Create a new prompt",
        keybind: "n",
        context: CommandContext::Library,
    },
    CommandSpec {
        id: CommandId::EditPrompt,
        title: "Edit Prompt",
        description: "Edit the selected prompt in $EDITOR",
        keybind: "e",
        context: CommandContext::Library,
    },
    CommandSpec {
        id: CommandId::FilterPrompts,
        title: "Filter Prompts",
        description: "Search the prompt library",
        keybind: "/",
        context: CommandContext::Library,
    },
    CommandSpec {
        id: CommandId::RefreshWorktrees,
        title: "Refresh Worktrees",
        description: "Re-scan the worktree list",
        keybind: "r",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::RunCustomPrompt,
        title: "Run Custom Prompt",
        description: "Run a free-text prompt on the selected worktree",
        keybind: "c",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::AddressPrComments,
        title: "Address PR Comments",
        description: "Address all open PR review comments",
        keybind: "p",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::CheckCi,
        title: "Check CI",
        description: "Check CI for failures and address them",
        keybind: "i",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::ToggleAutoContinue,
        title: "Toggle Auto-Continue",
        description: "Toggle auto-continue on PR merge",
        keybind: "a",
        context: CommandContext::Grid,
    },
];

/// Look up the [`CommandSpec`] for a [`CommandId`].
///
/// Panics only if the registry is missing the variant, which the
/// `registry_lists_every_command_once` test prevents.
pub fn spec(id: CommandId) -> &'static CommandSpec {
    ALL_COMMANDS
        .iter()
        .find(|s| s.id == id)
        .expect("every CommandId must be present in ALL_COMMANDS (see commands.rs tests)")
}

// ---------------------------------------------------------------------------
// Fuzzy matching
// ---------------------------------------------------------------------------

/// Case-insensitive subsequence test: does every char of `needle` appear in
/// `haystack` in order (gaps allowed)?  An empty needle always matches.
pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    'outer: for nc in needle.chars() {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Rank a spec against a lowercased query.  Lower is better; `None` means no
/// match.  Ranks (imbuia-style):
///   0 = title prefix
///   1 = title substring
///   2 = title subsequence
///   3 = description substring
///   4 = description subsequence
fn rank(spec: &CommandSpec, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let title = spec.title.to_lowercase();
    let desc = spec.description.to_lowercase();

    if title.starts_with(query) {
        Some(0)
    } else if title.contains(query) {
        Some(1)
    } else if is_subsequence(query, &title) {
        Some(2)
    } else if desc.contains(query) {
        Some(3)
    } else if is_subsequence(query, &desc) {
        Some(4)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Command-palette modal state.
pub struct Palette {
    /// The current filter query (raw, as typed).
    pub query: String,
    /// Commands eligible in this palette (Global + the opening view's context).
    pub visible: Vec<CommandId>,
    /// Indices into `visible`, ranked by the current query.
    pub filtered: Vec<usize>,
    /// Cursor position within `filtered`.
    pub cursor: usize,
}

impl Palette {
    /// Build a palette for the current view.  `visible` = commands whose
    /// context is `Global` or matches the opening view.
    pub fn open(view_is_grid: bool) -> Palette {
        let view_ctx = if view_is_grid {
            CommandContext::Grid
        } else {
            CommandContext::Library
        };
        let visible: Vec<CommandId> = ALL_COMMANDS
            .iter()
            .filter(|s| s.context == CommandContext::Global || s.context == view_ctx)
            .map(|s| s.id)
            .collect();
        let filtered: Vec<usize> = (0..visible.len()).collect();
        Palette {
            query: String::new(),
            visible,
            filtered,
            cursor: 0,
        }
    }

    /// Recompute `filtered` over `visible` for the current query and reset the
    /// cursor to the top.  Stable within a rank (preserves registry order).
    pub fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        let mut ranked: Vec<(usize, u8)> = self
            .visible
            .iter()
            .enumerate()
            .filter_map(|(i, &id)| rank(spec(id), &query).map(|r| (i, r)))
            .collect();
        // Stable sort by rank; equal ranks keep their registry/visible order.
        ranked.sort_by_key(|&(_, r)| r);
        self.filtered = ranked.into_iter().map(|(i, _)| i).collect();
        self.cursor = 0;
    }

    /// Move the cursor by `delta`, clamped to `[0, filtered.len() - 1]`.
    pub fn move_cursor(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.filtered.len() as i32 - 1;
        let next = (self.cursor as i32 + delta).clamp(0, max);
        self.cursor = next as usize;
    }

    /// The currently highlighted command, if any.
    pub fn selected(&self) -> Option<CommandId> {
        let visible_idx = *self.filtered.get(self.cursor)?;
        self.visible.get(visible_idx).copied()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every CommandId we expect the registry to cover.  This list is matched
    /// exhaustively below, so adding a variant without updating it is a compile
    /// error here too.
    fn all_ids() -> Vec<CommandId> {
        use CommandId::*;
        let all = [
            SwitchView,
            ToggleHelp,
            Quit,
            StopDaemon,
            NewPrompt,
            EditPrompt,
            FilterPrompts,
            RefreshWorktrees,
            RunCustomPrompt,
            AddressPrComments,
            CheckCi,
            ToggleAutoContinue,
        ];
        // Exhaustive match: a new variant forces a compile error here.
        for id in all {
            match id {
                SwitchView | ToggleHelp | Quit | StopDaemon | NewPrompt | EditPrompt
                | FilterPrompts | RefreshWorktrees | RunCustomPrompt | AddressPrComments
                | CheckCi | ToggleAutoContinue => {}
            }
        }
        all.to_vec()
    }

    #[test]
    fn registry_lists_every_command_once() {
        for id in all_ids() {
            let count = ALL_COMMANDS.iter().filter(|s| s.id == id).count();
            assert_eq!(count, 1, "{id:?} must appear in ALL_COMMANDS exactly once");
        }
        // No extra/duplicate entries beyond the known set.
        assert_eq!(ALL_COMMANDS.len(), all_ids().len());
    }

    #[test]
    fn titles_and_descriptions_non_empty() {
        for s in ALL_COMMANDS {
            assert!(!s.title.is_empty(), "{:?} has empty title", s.id);
            assert!(
                !s.description.is_empty(),
                "{:?} has empty description",
                s.id
            );
        }
    }

    #[test]
    fn spec_round_trips_every_id() {
        for id in all_ids() {
            assert_eq!(spec(id).id, id);
        }
    }

    #[test]
    fn is_subsequence_basic() {
        assert!(is_subsequence("", "anything"));
        assert!(is_subsequence("abc", "aXbXc"));
        assert!(is_subsequence("ce", "check ci"));
        assert!(!is_subsequence("cba", "abc"));
        assert!(!is_subsequence("abcd", "abc"));
    }

    #[test]
    fn rank_ordering() {
        // Title prefix beats substring beats subsequence beats description.
        let prefix = CommandSpec {
            id: CommandId::Quit,
            title: "quit",
            description: "zzz",
            keybind: "q",
            context: CommandContext::Global,
        };
        let substring = CommandSpec {
            title: "requit",
            ..prefix
        };
        let subseq = CommandSpec {
            title: "q-u-i-t-x",
            ..prefix
        };
        let desc = CommandSpec {
            title: "zzz",
            description: "quit now",
            ..prefix
        };
        assert_eq!(rank(&prefix, "quit"), Some(0));
        assert_eq!(rank(&substring, "quit"), Some(1));
        assert_eq!(rank(&subseq, "quit"), Some(2));
        assert_eq!(rank(&desc, "quit"), Some(3));
        assert_eq!(rank(&prefix, ""), Some(0));
        assert_eq!(rank(&prefix, "xyz"), None);
    }

    #[test]
    fn refilter_orders_by_rank_then_registry_order() {
        // Query "qu": only "Quit" has a title prefix match (rank 0); it must
        // come first ahead of any substring/subsequence/description matches.
        let mut p = Palette::open(true);
        p.query = "qu".to_string();
        p.refilter();
        let first = p.selected().expect("a match");
        assert_eq!(first, CommandId::Quit);
        assert_eq!(p.cursor, 0);

        // Ranks must be non-decreasing across the filtered list.
        let query = p.query.to_lowercase();
        let ranks: Vec<u8> = p
            .filtered
            .iter()
            .map(|&i| rank(spec(p.visible[i]), &query).expect("matched"))
            .collect();
        assert!(
            ranks.windows(2).all(|w| w[0] <= w[1]),
            "ranks must be sorted ascending: {ranks:?}"
        );
    }

    #[test]
    fn refilter_empty_query_keeps_all() {
        let mut p = Palette::open(true);
        let n = p.visible.len();
        p.refilter();
        assert_eq!(p.filtered.len(), n);
    }

    #[test]
    fn move_cursor_clamps() {
        let mut p = Palette::open(false);
        p.refilter();
        let last = p.filtered.len() - 1;
        p.move_cursor(-5);
        assert_eq!(p.cursor, 0);
        p.move_cursor(1000);
        assert_eq!(p.cursor, last);
        p.move_cursor(-1);
        assert_eq!(p.cursor, last - 1);
    }

    #[test]
    fn open_library_excludes_grid_commands() {
        let p = Palette::open(false);
        assert!(p.visible.contains(&CommandId::NewPrompt)); // library
        assert!(p.visible.contains(&CommandId::SwitchView)); // global
        assert!(!p.visible.contains(&CommandId::CheckCi)); // grid-only
        assert!(!p.visible.contains(&CommandId::RefreshWorktrees));
    }

    #[test]
    fn open_grid_excludes_library_commands() {
        let p = Palette::open(true);
        assert!(p.visible.contains(&CommandId::CheckCi)); // grid
        assert!(p.visible.contains(&CommandId::Quit)); // global
        assert!(!p.visible.contains(&CommandId::NewPrompt)); // library-only
        assert!(!p.visible.contains(&CommandId::FilterPrompts));
    }

    #[test]
    fn selected_none_when_no_matches() {
        let mut p = Palette::open(false);
        p.query = "zzzzznomatch".to_string();
        p.refilter();
        assert_eq!(p.selected(), None);
    }
}
