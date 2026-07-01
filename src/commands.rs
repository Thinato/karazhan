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
    /// Reload the prompt library from disk, picking up new/changed prompt files
    /// (Library; same as `r`).
    RefreshPrompts,
    /// Run a custom free-text prompt on the selected worktree (Grid; same as `c`).
    RunCustomPrompt,
    /// Address all open PR review comments on the selection (Grid; same as `p`).
    AddressPrComments,
    /// Check CI failures on the selection (Grid; same as `i`).
    CheckCi,
    /// Toggle auto-continue on PR merge for the selection (Grid; same as `a`).
    ToggleAutoContinue,
    /// Create a new worktree, blank or from a prompt (Grid; same as `n`).
    NewWorktree,
    /// Rename the selected worktree (Grid; same as `N`).
    RenameWorktree,
    /// Register a git repo as a new project (Global; same as `A`).
    AddProject,
    /// Delete the selected worktree with a (y/N) confirmation (Grid; same as `d`).
    DeleteWorktree,
    /// Open the selected worktree's PR in the default browser (Grid; same as `o`).
    OpenPr,
    /// Copy the selected worktree's PR URL to the system clipboard (Grid; same as `y`).
    CopyPrUrl,
    /// Copy `"<PR URL> - <PR title>"` to the system clipboard (Grid; same as `Y`).
    CopyPrUrlWithTitle,
    /// Create a new worktree from the selected library prompt in its own project
    /// (Library Normal mode; same as `Enter`).
    NewWorktreeFromPrompt,
    /// Resume the selected worktree's agent session to recover an errored /
    /// interrupted run (Grid; same as `R`).
    ResumeSession,
    /// Copy a shell command that `cd`s into the worktree and resumes its agent
    /// session, so the user can debug it in their own terminal (Grid; same as `s`).
    CopyResumeCommand,
    /// Widen the worktree-detail pane by 5 columns (Grid; same as `<`).
    WidenDetail,
    /// Narrow the worktree-detail pane by 5 columns (Grid; same as `>`).
    NarrowDetail,
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
        id: CommandId::RefreshPrompts,
        title: "Refresh prompts",
        description: "reload the prompt library from disk (pick up new prompts)",
        keybind: "r",
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
    CommandSpec {
        id: CommandId::NewWorktree,
        title: "New worktree",
        description: "create a worktree (blank or from a prompt)",
        keybind: "n",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::RenameWorktree,
        title: "Rename worktree",
        description: "rename the selected worktree",
        keybind: "N",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::AddProject,
        title: "Add project",
        description: "register a git repo as a project",
        keybind: "A",
        context: CommandContext::Global,
    },
    CommandSpec {
        id: CommandId::DeleteWorktree,
        title: "Delete worktree",
        description: "delete the selected worktree (with confirmation)",
        keybind: "d",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::OpenPr,
        title: "Open PR in browser",
        description: "open the worktree's PR in your default browser",
        keybind: "o",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::CopyPrUrl,
        title: "Copy PR URL",
        description: "copy the worktree's PR URL to the clipboard",
        keybind: "y",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::CopyPrUrlWithTitle,
        title: "Copy PR URL + title",
        description: "copy '<URL> - <title>' to the clipboard",
        keybind: "Y",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::NewWorktreeFromPrompt,
        title: "New worktree from prompt",
        description: "create a worktree from the selected prompt (its project)",
        keybind: "Enter",
        context: CommandContext::Library,
    },
    CommandSpec {
        id: CommandId::ResumeSession,
        title: "Resume session",
        description: "resume the worktree's session (recover an errored/interrupted run)",
        keybind: "R",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::CopyResumeCommand,
        title: "Copy resume command",
        description: "copy a 'cd <worktree> && resume session' shell command to debug it yourself",
        keybind: "s",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::WidenDetail,
        title: "Widen detail pane",
        description: "widen the worktree-detail pane by 5 cols (Ctrl-< for 1)",
        keybind: "<",
        context: CommandContext::Grid,
    },
    CommandSpec {
        id: CommandId::NarrowDetail,
        title: "Narrow detail pane",
        description: "narrow the worktree-detail pane by 5 cols (Ctrl-> for 1)",
        keybind: ">",
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
// New-worktree modal
// ---------------------------------------------------------------------------

/// Rank a lowercased `text` against a lowercased `query`.  Lower is better;
/// `None` means no match.  Mirrors the palette's title ranking:
///   0 = prefix, 1 = substring, 2 = subsequence.
fn rank_text(text: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    if text.starts_with(query) {
        Some(0)
    } else if text.contains(query) {
        Some(1)
    } else if is_subsequence(query, text) {
        Some(2)
    } else {
        None
    }
}

/// One selectable option in the new-worktree modal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeChoice {
    /// Create a detached worktree with no prompt run.
    Blank,
    /// Create a detached worktree and run this prompt's body on it.
    Prompt {
        slug: String,
        title: String,
        body: String,
    },
}

impl WorktreeChoice {
    /// The label shown in the modal list.
    fn label(&self) -> &str {
        match self {
            WorktreeChoice::Blank => "blank worktree",
            WorktreeChoice::Prompt { title, .. } => title,
        }
    }
}

/// Which phase the two-phase new-worktree modal is currently in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewWorktreePhase {
    /// Pick which project the new worktree goes in (only when >1 project).
    PickProject,
    /// Pick the blank/prompt choice for the (already-selected) project.
    PickChoice,
}

/// New-worktree modal state (key `n` in Grid view).
///
/// Two phases: first pick a project (skipped when there is exactly one), then
/// pick "blank worktree" (always first) or a library prompt (by title).  The
/// query/cursor/filter machinery applies to whichever phase's list is active.
pub struct NewWorktreeModal {
    /// The current filter query (raw, as typed).  Reset when switching phases.
    pub query: String,
    /// Cursor position within `filtered`.
    pub cursor: usize,
    /// Indices into the active phase's list, ranked by the current query.
    pub filtered: Vec<usize>,
    /// Current phase.
    phase: NewWorktreePhase,
    /// Whether a project step exists (true when >1 project was supplied).  When
    /// false, Esc in PickChoice closes instead of going back.
    has_project_step: bool,
    /// Available project names (PickProject list source).
    projects: Vec<String>,
    /// The chosen project (preselected when exactly one project).
    selected_project: Option<String>,
    /// Choice options; index 0 is always [`WorktreeChoice::Blank`].
    options: Vec<WorktreeChoice>,
}

impl NewWorktreeModal {
    /// Build a modal from the available `projects` and the library's prompt
    /// choices `(slug, title, body)`.  "blank worktree" is always the first
    /// choice.  With exactly one project the modal starts in `PickChoice` with
    /// that project preselected; with more than one it starts in `PickProject`.
    /// Callers must guard against an empty `projects` list (the command should
    /// not open the modal at all in that case).
    pub fn new(projects: Vec<String>, prompt_choices: Vec<(String, String, String)>) -> Self {
        let mut options = vec![WorktreeChoice::Blank];
        options.extend(
            prompt_choices
                .into_iter()
                .map(|(slug, title, body)| WorktreeChoice::Prompt { slug, title, body }),
        );

        let single = projects.len() == 1;
        let (phase, selected_project) = if single {
            (NewWorktreePhase::PickChoice, projects.first().cloned())
        } else {
            (NewWorktreePhase::PickProject, None)
        };

        let mut modal = Self {
            query: String::new(),
            cursor: 0,
            filtered: Vec::new(),
            phase,
            has_project_step: !single,
            projects,
            selected_project,
            options,
        };
        modal.refilter();
        modal
    }

    /// The current phase.
    pub fn phase(&self) -> NewWorktreePhase {
        self.phase
    }

    /// Replace the choice options from a project's prompt list `(slug, title,
    /// body)`.  "blank worktree" remains the first option.  Used to scope the
    /// `PickChoice` list to the project picked in `PickProject` before advancing.
    pub fn set_choices(&mut self, prompt_choices: Vec<(String, String, String)>) {
        let mut options = vec![WorktreeChoice::Blank];
        options.extend(
            prompt_choices
                .into_iter()
                .map(|(slug, title, body)| WorktreeChoice::Prompt { slug, title, body }),
        );
        self.options = options;
    }

    /// The selected project, once chosen (always `Some` in `PickChoice`).
    pub fn selected_project(&self) -> Option<&str> {
        self.selected_project.as_deref()
    }

    /// The labels of the active phase's filtered rows, in display order, with a
    /// flag marking the highlighted row.  Used by the renderer.
    pub fn filtered_rows(&self) -> Vec<(String, bool)> {
        self.filtered
            .iter()
            .enumerate()
            .filter_map(|(row, &idx)| {
                let label = match self.phase {
                    NewWorktreePhase::PickProject => self.projects.get(idx).cloned()?,
                    NewWorktreePhase::PickChoice => self.options.get(idx)?.label().to_string(),
                };
                Some((label, row == self.cursor))
            })
            .collect()
    }

    /// Recompute `filtered` for the active phase + current query, resetting the
    /// cursor.  "blank worktree" matches an empty query and the literal "blank".
    pub fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        let mut ranked: Vec<(usize, u8)> = match self.phase {
            NewWorktreePhase::PickProject => self
                .projects
                .iter()
                .enumerate()
                .filter_map(|(i, name)| rank_text(&name.to_lowercase(), &query).map(|r| (i, r)))
                .collect(),
            NewWorktreePhase::PickChoice => self
                .options
                .iter()
                .enumerate()
                .filter_map(|(i, choice)| {
                    rank_text(&choice.label().to_lowercase(), &query).map(|r| (i, r))
                })
                .collect(),
        };
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

    /// The highlighted project name in `PickProject`, if any.
    pub fn selected_project_row(&self) -> Option<&str> {
        let idx = *self.filtered.get(self.cursor)?;
        self.projects.get(idx).map(String::as_str)
    }

    /// The currently highlighted choice in `PickChoice`, if any.
    pub fn selected_choice(&self) -> Option<&WorktreeChoice> {
        if self.phase != NewWorktreePhase::PickChoice {
            return None;
        }
        let opt_idx = *self.filtered.get(self.cursor)?;
        self.options.get(opt_idx)
    }

    /// Advance from `PickProject` to `PickChoice`, recording the highlighted
    /// project.  No-op when already in `PickChoice` or no project is highlighted.
    /// Returns `true` if it advanced.
    pub fn advance_to_choice(&mut self) -> bool {
        if self.phase != NewWorktreePhase::PickProject {
            return false;
        }
        let Some(project) = self.selected_project_row().map(str::to_string) else {
            return false;
        };
        self.selected_project = Some(project);
        self.phase = NewWorktreePhase::PickChoice;
        self.query.clear();
        self.refilter();
        true
    }

    /// Go back from `PickChoice` to `PickProject` (clearing the chosen project).
    /// Returns `true` if it went back; `false` when there is no project step
    /// (the caller should then close the modal).
    pub fn back_to_project(&mut self) -> bool {
        if !self.has_project_step || self.phase != NewWorktreePhase::PickChoice {
            return false;
        }
        self.phase = NewWorktreePhase::PickProject;
        self.selected_project = None;
        self.query.clear();
        self.refilter();
        true
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
            RefreshPrompts,
            RefreshWorktrees,
            RunCustomPrompt,
            AddressPrComments,
            CheckCi,
            ToggleAutoContinue,
            NewWorktree,
            RenameWorktree,
            AddProject,
            DeleteWorktree,
            OpenPr,
            CopyPrUrl,
            CopyPrUrlWithTitle,
            NewWorktreeFromPrompt,
            ResumeSession,
            CopyResumeCommand,
            WidenDetail,
            NarrowDetail,
        ];
        // Exhaustive match: a new variant forces a compile error here.
        for id in all {
            match id {
                SwitchView
                | ToggleHelp
                | Quit
                | StopDaemon
                | NewPrompt
                | EditPrompt
                | FilterPrompts
                | RefreshPrompts
                | RefreshWorktrees
                | RunCustomPrompt
                | AddressPrComments
                | CheckCi
                | ToggleAutoContinue
                | NewWorktree
                | RenameWorktree
                | AddProject
                | DeleteWorktree
                | OpenPr
                | CopyPrUrl
                | CopyPrUrlWithTitle
                | NewWorktreeFromPrompt
                | ResumeSession
                | CopyResumeCommand
                | WidenDetail
                | NarrowDetail => {}
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

    // ── NewWorktreeModal ──────────────────────────────────────────────────────

    /// Single-project modal (starts in PickChoice) over the given prompts.
    fn modal_with(prompts: &[(&str, &str)]) -> NewWorktreeModal {
        NewWorktreeModal::new(vec!["solo".to_string()], choices_of(prompts))
    }

    /// Multi-project modal (starts in PickProject) over the given prompts.
    fn multi_modal(projects: &[&str], prompts: &[(&str, &str)]) -> NewWorktreeModal {
        let projects = projects.iter().map(|p| (*p).to_string()).collect();
        NewWorktreeModal::new(projects, choices_of(prompts))
    }

    fn choices_of(prompts: &[(&str, &str)]) -> Vec<(String, String, String)> {
        prompts
            .iter()
            .map(|(slug, title)| {
                (
                    (*slug).to_string(),
                    (*title).to_string(),
                    format!("body of {slug}"),
                )
            })
            .collect()
    }

    #[test]
    fn modal_single_project_starts_in_pick_choice_preselected() {
        let m = modal_with(&[("refactor", "Refactor parser")]);
        assert_eq!(m.phase(), NewWorktreePhase::PickChoice);
        assert_eq!(m.selected_project(), Some("solo"));
        // Empty query keeps everything, blank first.
        assert_eq!(m.selected_choice(), Some(&WorktreeChoice::Blank));
    }

    #[test]
    fn modal_multi_project_starts_in_pick_project() {
        let m = multi_modal(&["alpha", "beta"], &[("refactor", "Refactor parser")]);
        assert_eq!(m.phase(), NewWorktreePhase::PickProject);
        assert_eq!(m.selected_project(), None);
        // PickChoice accessor is inert before a project is chosen.
        assert_eq!(m.selected_choice(), None);
        assert_eq!(m.selected_project_row(), Some("alpha"));
    }

    #[test]
    fn modal_multi_project_enter_advances_and_sets_project() {
        let mut m = multi_modal(&["alpha", "beta"], &[("refactor", "Refactor parser")]);
        m.move_cursor(1); // highlight "beta"
        assert_eq!(m.selected_project_row(), Some("beta"));
        assert!(m.advance_to_choice());
        assert_eq!(m.phase(), NewWorktreePhase::PickChoice);
        assert_eq!(m.selected_project(), Some("beta"));
        // Now choice list is active, blank first.
        assert_eq!(m.selected_choice(), Some(&WorktreeChoice::Blank));
    }

    #[test]
    fn modal_fuzzy_filter_in_project_phase() {
        let mut m = multi_modal(&["karazhan", "imbuia"], &[]);
        m.query = "imb".to_string();
        m.refilter();
        assert_eq!(m.selected_project_row(), Some("imbuia"));
    }

    #[test]
    fn modal_filters_by_title_in_choice_phase() {
        let mut m = modal_with(&[("refactor", "Refactor parser"), ("docs", "Write docs")]);
        m.query = "docs".to_string();
        m.refilter();
        match m.selected_choice().expect("a match") {
            WorktreeChoice::Prompt { slug, .. } => assert_eq!(slug, "docs"),
            other => panic!("expected docs prompt, got {other:?}"),
        }
    }

    #[test]
    fn modal_blank_choice_matches_literal_blank_query() {
        let mut m = modal_with(&[("refactor", "Refactor parser")]);
        m.query = "blank".to_string();
        m.refilter();
        assert_eq!(m.selected_choice(), Some(&WorktreeChoice::Blank));
    }

    #[test]
    fn modal_selecting_prompt_choice() {
        let mut m = modal_with(&[("refactor", "Refactor parser")]);
        m.query = "refactor".to_string();
        m.refilter();
        match m.selected_choice().expect("match") {
            WorktreeChoice::Prompt { slug, title, body } => {
                assert_eq!(slug, "refactor");
                assert_eq!(title, "Refactor parser");
                assert_eq!(body, "body of refactor");
            }
            other => panic!("expected prompt, got {other:?}"),
        }
    }

    #[test]
    fn modal_no_match_selects_none() {
        let mut m = modal_with(&[("refactor", "Refactor parser")]);
        m.query = "zzzznope".to_string();
        m.refilter();
        assert_eq!(m.selected_choice(), None);
    }

    #[test]
    fn modal_esc_back_from_choice_to_project() {
        let mut m = multi_modal(&["alpha", "beta"], &[("refactor", "Refactor parser")]);
        assert!(m.advance_to_choice());
        assert_eq!(m.phase(), NewWorktreePhase::PickChoice);
        assert!(m.back_to_project());
        assert_eq!(m.phase(), NewWorktreePhase::PickProject);
        assert_eq!(m.selected_project(), None);
    }

    #[test]
    fn modal_single_project_has_no_back_step() {
        let mut m = modal_with(&[("refactor", "Refactor parser")]);
        // No project step → back_to_project is a no-op (caller closes instead).
        assert!(!m.back_to_project());
        assert_eq!(m.phase(), NewWorktreePhase::PickChoice);
    }

    #[test]
    fn modal_choices_reflect_picked_project() {
        // Multi-project modal opens with NO choices; the app fills them per the
        // picked project via set_choices before advancing.
        let mut m = multi_modal(&["alpha", "beta"], &[]);

        // Pick "alpha" → scope to alpha's prompts.
        assert_eq!(m.selected_project_row(), Some("alpha"));
        m.set_choices(choices_of(&[("a1", "Alpha One")]));
        assert!(m.advance_to_choice());
        let labels: Vec<String> = m.filtered_rows().into_iter().map(|(l, _)| l).collect();
        assert!(labels.contains(&"blank worktree".to_string()));
        assert!(labels.contains(&"Alpha One".to_string()));
        assert!(!labels.contains(&"Beta One".to_string()));

        // Go back, pick "beta" → scope to beta's prompts (different set).
        assert!(m.back_to_project());
        m.move_cursor(1); // highlight "beta"
        assert_eq!(m.selected_project_row(), Some("beta"));
        m.set_choices(choices_of(&[("b1", "Beta One")]));
        assert!(m.advance_to_choice());
        let labels: Vec<String> = m.filtered_rows().into_iter().map(|(l, _)| l).collect();
        assert!(labels.contains(&"blank worktree".to_string())); // blank always present
        assert!(labels.contains(&"Beta One".to_string()));
        assert!(!labels.contains(&"Alpha One".to_string()));
    }
}
