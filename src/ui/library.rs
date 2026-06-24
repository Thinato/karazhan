//! Prompt library view — grouped by project.
//!
//! Migration note: prompts now live PER-PROJECT at
//! `<project_root>/.karazhan/prompts/<slug>.md` (committed, shipped with each
//! repo).  The previous GLOBAL `<cwd>/prompts` directory is no longer read; old
//! prompts there are not auto-migrated — move them into the relevant project's
//! `.karazhan/prompts/` by hand if you still want them.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::ipc::ProjectInfo;
use crate::prompts::{model::slugify, Prompt, PromptStore};

// ---------------------------------------------------------------------------
// Input mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryMode {
    /// Normal browsing.
    Normal,
    /// User pressed `/` — typing a search query.
    Filter,
    /// User is choosing which project a new prompt belongs to (only when there
    /// is more than one project).  Enter advances to [`LibraryMode::NewPrompt`].
    NewPromptProject,
    /// User is typing a title for a new prompt (project already chosen).
    NewPrompt,
}

// ---------------------------------------------------------------------------
// SelectedPrompt
// ---------------------------------------------------------------------------

/// The identifying data of the currently selected prompt in the library.
/// Returned by [`LibraryView::selected_prompt`].
pub struct SelectedPrompt {
    /// Name of the project that owns this prompt.
    pub project: String,
    /// Filesystem slug (used as the prompt filename without `.md`).
    pub slug: String,
    /// Human-readable title.
    pub title: String,
    /// Full prompt body text.
    pub body: String,
}

// ---------------------------------------------------------------------------
// Per-project prompt store + loaded prompts
// ---------------------------------------------------------------------------

/// One project's prompts, loaded from `<project_root>/.karazhan/prompts/`.
struct LibraryProject {
    name: String,
    store: PromptStore,
    prompts: Vec<Prompt>,
}

impl LibraryProject {
    /// Build a project's store + load its prompts (missing dir → empty list).
    fn load(info: &ProjectInfo) -> Self {
        let dir = info.path.join(".karazhan").join("prompts");
        let store = PromptStore::new(dir);
        let prompts = store.load_all().unwrap_or_else(|e| {
            tracing::debug!("library: no prompts for {}: {e}", info.name);
            vec![]
        });
        Self {
            name: info.name.clone(),
            store,
            prompts,
        }
    }
}

// ---------------------------------------------------------------------------
// Flat-row addressing
// ---------------------------------------------------------------------------

/// A flattened reference into the per-project prompt lists: which project, and
/// which prompt within it.  The flat list is the concatenation of every
/// project's (filtered) prompts in project order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FlatRef {
    project: usize,
    prompt: usize,
}

// ---------------------------------------------------------------------------
// LibraryView
// ---------------------------------------------------------------------------

pub struct LibraryView {
    /// Per-project prompt stores, in project order (matches Snapshot order).
    projects: Vec<LibraryProject>,
    /// Flat selection list over the concatenated (filtered) prompts, in project
    /// order.  Drives navigation + the list highlight.
    flat: Vec<FlatRef>,
    /// Selection index into `flat`.
    selected: Option<usize>,
    pub mode: LibraryMode,
    /// Current text typed in filter or new-prompt input box.
    pub input: String,
    /// Status message shown at the bottom (clears on next keypress).
    pub status: Option<String>,
    /// Project index chosen for a pending new prompt (set in `NewPrompt`).
    new_prompt_project: Option<usize>,
}

impl LibraryView {
    /// Build an empty library.  Prompts are populated per-project once the first
    /// Snapshot arrives via [`LibraryView::set_projects`].
    pub fn new() -> Self {
        Self {
            projects: Vec::new(),
            flat: Vec::new(),
            selected: None,
            mode: LibraryMode::Normal,
            input: String::new(),
            status: None,
            new_prompt_project: None,
        }
    }

    // -----------------------------------------------------------------------
    // Project wiring
    // -----------------------------------------------------------------------

    /// Rebuild the per-project stores from a fresh project list, loading each
    /// project's prompts.  Preserves the current selection by (project, slug)
    /// when possible.
    pub fn set_projects(&mut self, projects: &[ProjectInfo]) {
        let prev = self.selected_project_slug();

        self.projects = projects.iter().map(LibraryProject::load).collect();
        self.apply_filter();
        self.restore_selection(prev);
    }

    /// Reload every project's prompts from disk, keeping the selection by
    /// (project, slug) when it still exists.
    pub fn reload_keep_selection(&mut self) {
        let prev = self.selected_project_slug();
        for proj in &mut self.projects {
            proj.prompts = proj.store.load_all().unwrap_or_else(|e| {
                tracing::debug!("library: reload failed for {}: {e}", proj.name);
                vec![]
            });
        }
        self.apply_filter();
        self.restore_selection(prev);
    }

    /// The (project_name, slug) of the current selection, if any.
    fn selected_project_slug(&self) -> Option<(String, String)> {
        let r = self.flat.get(self.selected?)?;
        let proj = self.projects.get(r.project)?;
        let prompt = proj.prompts.get(r.prompt)?;
        Some((proj.name.clone(), prompt.slug.clone()))
    }

    /// Restore selection to a (project_name, slug) pair if it is still present
    /// in the current flat list; otherwise clamp to the first row.
    fn restore_selection(&mut self, prev: Option<(String, String)>) {
        if let Some((name, slug)) = prev {
            if let Some(pos) = self.flat.iter().position(|r| {
                self.projects
                    .get(r.project)
                    .is_some_and(|p| p.name == name && p.prompts[r.prompt].slug == slug)
            }) {
                self.selected = Some(pos);
                return;
            }
        }
        self.selected = if self.flat.is_empty() { None } else { Some(0) };
    }

    // -----------------------------------------------------------------------
    // Per-project prompt access (new-worktree modal)
    // -----------------------------------------------------------------------

    /// The prompts for one project as `(slug, title, body)` tuples, used to
    /// build the new-worktree modal choices.  Empty for an unknown project.
    pub fn prompts_for_project(&self, project_name: &str) -> Vec<(String, String, String)> {
        self.projects
            .iter()
            .find(|p| p.name == project_name)
            .map(|p| {
                p.prompts
                    .iter()
                    .map(|pr| (pr.slug.clone(), pr.title.clone(), pr.body.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Filesystem path of the currently selected prompt, if any.
    pub fn selected_prompt_path(&self) -> Option<std::path::PathBuf> {
        let r = self.flat.get(self.selected?)?;
        let proj = self.projects.get(r.project)?;
        let slug = &proj.prompts.get(r.prompt)?.slug;
        Some(proj.store.path_for(slug))
    }

    /// The currently selected prompt's identifying data (project name, slug,
    /// title, body), or `None` when nothing is selected or the library is empty.
    pub fn selected_prompt(&self) -> Option<SelectedPrompt> {
        let r = self.flat.get(self.selected?)?;
        let proj = self.projects.get(r.project)?;
        let prompt = proj.prompts.get(r.prompt)?;
        Some(SelectedPrompt {
            project: proj.name.clone(),
            slug: prompt.slug.clone(),
            title: prompt.title.clone(),
            body: prompt.body.clone(),
        })
    }

    // -----------------------------------------------------------------------
    // Filtering / flat-list construction
    // -----------------------------------------------------------------------

    /// Rebuild `flat` from the current projects + filter query (applies across
    /// all projects, still in project order).  Clamps the selection.
    fn apply_filter(&mut self) {
        let query = if self.mode == LibraryMode::Filter {
            self.input.as_str()
        } else {
            ""
        };

        let mut flat = Vec::new();
        for (pi, proj) in self.projects.iter().enumerate() {
            for matched in PromptStore::search(&proj.prompts, query) {
                if let Some(prompt) = proj.prompts.iter().position(|p| std::ptr::eq(p, matched)) {
                    flat.push(FlatRef {
                        project: pi,
                        prompt,
                    });
                }
            }
        }
        self.flat = flat;

        // Clamp selection.
        if self.flat.is_empty() {
            self.selected = None;
        } else {
            let sel = self.selected.unwrap_or(0).min(self.flat.len() - 1);
            self.selected = Some(sel);
        }
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    pub fn move_down(&mut self) {
        if self.flat.is_empty() {
            return;
        }
        let i = self.selected.unwrap_or(0);
        self.selected = Some((i + 1).min(self.flat.len() - 1));
    }

    pub fn move_up(&mut self) {
        if self.flat.is_empty() {
            return;
        }
        let i = self.selected.unwrap_or(0);
        self.selected = Some(i.saturating_sub(1));
    }

    // -----------------------------------------------------------------------
    // Filter mode
    // -----------------------------------------------------------------------

    pub fn enter_filter(&mut self) {
        self.mode = LibraryMode::Filter;
        self.input.clear();
        self.status = None;
        self.apply_filter();
    }

    pub fn filter_push(&mut self, ch: char) {
        self.input.push(ch);
        self.apply_filter();
    }

    pub fn filter_pop(&mut self) {
        self.input.pop();
        self.apply_filter();
    }

    pub fn clear_filter(&mut self) {
        self.mode = LibraryMode::Normal;
        self.input.clear();
        self.apply_filter();
    }

    // -----------------------------------------------------------------------
    // New prompt mode (with project pick)
    // -----------------------------------------------------------------------

    /// Begin creating a new prompt.  With zero projects: set a status and do
    /// nothing.  With one project: go straight to the title input for it.  With
    /// more than one: enter the project-pick step first.
    pub fn enter_new_prompt(&mut self) {
        self.input.clear();
        self.status = None;
        match self.projects.len() {
            0 => {
                self.status = Some("add a project first (A)".to_string());
            }
            1 => {
                self.new_prompt_project = Some(0);
                self.mode = LibraryMode::NewPrompt;
            }
            _ => {
                self.new_prompt_project = None;
                self.mode = LibraryMode::NewPromptProject;
            }
        }
    }

    /// In the project-pick step, move the highlighted project by `delta`.  The
    /// highlight reuses the flat selection index as a project cursor.
    pub fn new_prompt_project_move(&mut self, delta: i32) {
        if self.projects.is_empty() {
            return;
        }
        let max = self.projects.len() as i32 - 1;
        let cur = self.new_prompt_project.unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, max);
        self.new_prompt_project = Some(next as usize);
    }

    /// The currently highlighted project index in the project-pick step.
    pub fn new_prompt_project_cursor(&self) -> usize {
        self.new_prompt_project.unwrap_or(0)
    }

    /// Confirm the project pick, advancing to the title input.
    pub fn confirm_new_prompt_project(&mut self) {
        if self.projects.is_empty() {
            self.cancel_input();
            return;
        }
        let idx = self
            .new_prompt_project
            .unwrap_or(0)
            .min(self.projects.len() - 1);
        self.new_prompt_project = Some(idx);
        self.input.clear();
        self.mode = LibraryMode::NewPrompt;
    }

    pub fn new_prompt_push(&mut self, ch: char) {
        self.input.push(ch);
    }

    pub fn new_prompt_pop(&mut self) {
        self.input.pop();
    }

    /// Confirm the new prompt: slugify the title, create an empty Prompt, save
    /// it into the CHOSEN project's store, reload, and select it.  Returns an
    /// error string on failure.
    pub fn confirm_new_prompt(&mut self) -> Result<(), String> {
        let title = self.input.trim().to_string();
        if title.is_empty() {
            self.cancel_input();
            return Ok(());
        }
        let slug = slugify(&title);
        if slug.is_empty() {
            return Err("title produced an empty slug — use alphanumeric characters".to_string());
        }

        let Some(pi) = self.new_prompt_project else {
            return Err("no project selected".to_string());
        };
        let proj_name = match self.projects.get(pi) {
            Some(p) => p.name.clone(),
            None => return Err("no project selected".to_string()),
        };

        let prompt = Prompt {
            slug: slug.clone(),
            title: title.clone(),
            tags: vec![],
            vars: vec![],
            body: String::new(),
        };

        self.projects[pi]
            .store
            .save(&prompt)
            .map_err(|e| e.to_string())?;

        self.mode = LibraryMode::Normal;
        self.input.clear();
        self.new_prompt_project = None;

        // Reload that project's prompts + rebuild the flat list, then select the
        // freshly created prompt by (project, slug).
        self.projects[pi].prompts = self.projects[pi].store.load_all().unwrap_or_default();
        self.apply_filter();
        self.restore_selection(Some((proj_name, slug)));

        self.status = Some(format!("created \"{title}\""));
        Ok(())
    }

    pub fn cancel_input(&mut self) {
        self.mode = LibraryMode::Normal;
        self.input.clear();
        self.new_prompt_project = None;
    }

    // -----------------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------------

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Split into: list area + bottom bar.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);

        let title = match &self.mode {
            LibraryMode::Filter => format!(" karazhan — filter: {} ", self.input),
            _ => " karazhan — prompt library ".to_string(),
        };

        let list_block = Block::default()
            .title(title)
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan));

        if self.mode == LibraryMode::NewPromptProject {
            self.render_project_pick(frame, chunks[0], list_block);
        } else {
            self.render_prompt_list(frame, chunks[0], list_block);
        }

        // ---- Bottom bar ----
        let bottom_text = match &self.mode {
            LibraryMode::Normal => self
                .status
                .as_deref()
                .unwrap_or("j/k: move  /: filter  n: new  e: edit  q: quit")
                .to_string(),
            LibraryMode::Filter => {
                format!("filter: {}█  Esc: clear", self.input)
            }
            LibraryMode::NewPromptProject => {
                "pick a project for the new prompt  Enter: select  Esc: cancel".to_string()
            }
            LibraryMode::NewPrompt => {
                format!(
                    "new prompt title: {}█  Enter: confirm  Esc: cancel",
                    self.input
                )
            }
        };

        let bottom_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray));

        let bottom = Paragraph::new(bottom_text)
            .block(bottom_block)
            .alignment(Alignment::Left);

        frame.render_widget(bottom, chunks[1]);
    }

    /// Render the grouped-by-project prompt list with a flat selection highlight.
    fn render_prompt_list(&mut self, frame: &mut Frame, area: Rect, block: Block) {
        if self.projects.is_empty() {
            let msg = Paragraph::new(" No projects.  Add one with `A`.")
                .block(block)
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, area);
            return;
        }

        // Build list items grouped by project: a header + divider per project,
        // then its (filtered) prompts.  Track which list rows correspond to a
        // flat selection index so the highlight lands on the right prompt.
        let mut items: Vec<ListItem> = Vec::new();
        let mut row_to_flat: Vec<Option<usize>> = Vec::new();
        let inner_w = area.width.saturating_sub(2) as usize;

        let mut flat_idx = 0usize;
        for (pi, proj) in self.projects.iter().enumerate() {
            items.push(project_header_item(&proj.name, inner_w));
            row_to_flat.push(None);

            let proj_rows: Vec<usize> = self
                .flat
                .iter()
                .enumerate()
                .filter(|(_, r)| r.project == pi)
                .map(|(i, _)| i)
                .collect();

            if proj_rows.is_empty() {
                items.push(ListItem::new(Line::from(Span::styled(
                    "   (no prompts)",
                    Style::default().fg(Color::DarkGray),
                ))));
                row_to_flat.push(None);
                continue;
            }

            for fi in proj_rows {
                let prompt = &proj.prompts[self.flat[fi].prompt];
                let tags_str = if prompt.tags.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", prompt.tags.join(", "))
                };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("  {}", prompt.title),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(tags_str, Style::default().fg(Color::DarkGray)),
                ])));
                row_to_flat.push(Some(fi));
                flat_idx += 1;
            }
        }
        let _ = flat_idx;

        // Map the flat selection to a list-row index for the highlight.
        let selected_row = self
            .selected
            .and_then(|sel| row_to_flat.iter().position(|r| *r == Some(sel)));
        let mut list_state = ListState::default();
        list_state.select(selected_row);

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut list_state);
    }

    /// Render the project-pick step for a new prompt.
    fn render_project_pick(&mut self, frame: &mut Frame, area: Rect, block: Block) {
        let cursor = self.new_prompt_project_cursor();
        let items: Vec<ListItem> = self
            .projects
            .iter()
            .map(|p| {
                ListItem::new(Line::from(Span::styled(
                    p.name.clone(),
                    Style::default().fg(Color::White),
                )))
            })
            .collect();

        let mut list_state = ListState::default();
        if !self.projects.is_empty() {
            list_state.select(Some(cursor.min(self.projects.len() - 1)));
        }

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut list_state);
    }
}

impl Default for LibraryView {
    fn default() -> Self {
        Self::new()
    }
}

/// A project header list item: name + a `─`-filled divider spanning the width.
fn project_header_item(name: &str, width: usize) -> ListItem<'static> {
    let label = format!(" {name} ");
    let rule_len = width.saturating_sub(label.chars().count());
    let rule: String = "─".repeat(rule_len);
    ListItem::new(Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(rule, Style::default().fg(Color::DarkGray)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `ProjectInfo` over a fresh temp dir, saving the given prompts
    /// into `<dir>/.karazhan/prompts/`.  Returns the tempdir guard + info.
    fn project_with(name: &str, slugs: &[&str]) -> (tempfile::TempDir, ProjectInfo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let info = ProjectInfo {
            name: name.to_string(),
            path: dir.path().to_path_buf(),
        };
        let store = PromptStore::new(dir.path().join(".karazhan").join("prompts"));
        for slug in slugs {
            store
                .save(&Prompt {
                    slug: (*slug).to_string(),
                    title: (*slug).to_string(),
                    tags: vec![],
                    vars: vec![],
                    body: format!("body of {slug}"),
                })
                .expect("save");
        }
        (dir, info)
    }

    #[test]
    fn set_projects_builds_per_project_stores() {
        let (_d1, p1) = project_with("alpha", &["a1", "a2"]);
        let (_d2, p2) = project_with("beta", &["b1"]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1, p2]);
        assert_eq!(view.projects.len(), 2);
        assert_eq!(view.projects[0].prompts.len(), 2);
        assert_eq!(view.projects[1].prompts.len(), 1);
        // Flat list concatenates in project order: a1, a2, b1.
        assert_eq!(view.flat.len(), 3);
    }

    #[test]
    fn empty_project_appears_with_no_prompts() {
        let (_d1, p1) = project_with("alpha", &["a1"]);
        let (_d2, p2) = project_with("empty", &[]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1, p2]);
        // Flat list only has alpha's one prompt; empty contributes nothing.
        assert_eq!(view.flat.len(), 1);
        let names: Vec<String> = view.projects.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["alpha".to_string(), "empty".to_string()]);
    }

    #[test]
    fn flat_selection_maps_to_right_project_and_prompt() {
        let (_d1, p1) = project_with("alpha", &["a1", "a2"]);
        let (_d2, p2) = project_with("beta", &["b1"]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1.clone(), p2.clone()]);

        // First selection → alpha's first prompt (slugs sorted: a1, a2).
        let first = view.selected_prompt_path().expect("a selection");
        assert!(first.starts_with(&p1.path));
        assert!(first.to_string_lossy().contains("a1.md"));

        // Move to the third flat row → beta's b1.
        view.move_down();
        view.move_down();
        let third = view.selected_prompt_path().expect("a selection");
        assert!(third.starts_with(&p2.path));
        assert!(third.to_string_lossy().contains("b1.md"));
    }

    #[test]
    fn prompts_for_project_returns_only_that_project() {
        let (_d1, p1) = project_with("alpha", &["a1", "a2"]);
        let (_d2, p2) = project_with("beta", &["b1"]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1, p2]);

        let alpha = view.prompts_for_project("alpha");
        assert_eq!(alpha.len(), 2);
        assert!(alpha.iter().all(|(slug, _, _)| slug.starts_with('a')));

        let beta = view.prompts_for_project("beta");
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].0, "b1");

        assert!(view.prompts_for_project("nope").is_empty());
    }

    #[test]
    fn create_saves_into_chosen_project() {
        let (_d1, p1) = project_with("alpha", &[]);
        let (d2, p2) = project_with("beta", &[]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1, p2]);

        // Choose the second project (beta), then create.
        view.enter_new_prompt();
        assert_eq!(view.mode, LibraryMode::NewPromptProject);
        view.new_prompt_project_move(1); // highlight beta
        view.confirm_new_prompt_project();
        assert_eq!(view.mode, LibraryMode::NewPrompt);
        for ch in "My Prompt".chars() {
            view.new_prompt_push(ch);
        }
        view.confirm_new_prompt().expect("create");

        // The file landed under beta's .karazhan/prompts.
        let expected = d2
            .path()
            .join(".karazhan")
            .join("prompts")
            .join("my-prompt.md");
        assert!(expected.exists(), "prompt should exist at {expected:?}");
        assert_eq!(view.prompts_for_project("beta").len(), 1);
        assert_eq!(view.prompts_for_project("alpha").len(), 0);
    }

    #[test]
    fn create_single_project_skips_project_pick() {
        let (_d1, p1) = project_with("solo", &[]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1]);
        view.enter_new_prompt();
        assert_eq!(view.mode, LibraryMode::NewPrompt);
        for ch in "Hello".chars() {
            view.new_prompt_push(ch);
        }
        view.confirm_new_prompt().expect("create");
        assert_eq!(view.prompts_for_project("solo").len(), 1);
    }

    #[test]
    fn create_with_no_projects_sets_status() {
        let mut view = LibraryView::new();
        view.enter_new_prompt();
        assert_eq!(view.mode, LibraryMode::Normal);
        assert_eq!(view.status.as_deref(), Some("add a project first (A)"));
    }

    #[test]
    fn filter_applies_across_projects() {
        let (_d1, p1) = project_with("alpha", &["refactor"]);
        let (_d2, p2) = project_with("beta", &["refine", "docs"]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1, p2]);
        view.enter_filter();
        for ch in "ref".chars() {
            view.filter_push(ch);
        }
        // "refactor" (alpha) + "refine" (beta) match; "docs" does not.
        assert_eq!(view.flat.len(), 2);
    }

    #[test]
    fn reload_keep_selection_stays_on_same_prompt() {
        let (_d1, p1) = project_with("alpha", &["a1", "a2"]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1]);
        view.move_down(); // select a2
        let before = view.selected_prompt_path().expect("selection");
        view.reload_keep_selection();
        assert_eq!(view.selected_prompt_path(), Some(before));
    }

    #[test]
    fn selected_prompt_path_none_when_empty() {
        let view = LibraryView::new();
        assert_eq!(view.selected_prompt_path(), None);
    }

    #[test]
    fn selected_prompt_returns_right_project_slug_title_body() {
        let (_d1, p1) = project_with("alpha", &["a1", "a2"]);
        let (_d2, p2) = project_with("beta", &["b1"]);
        let mut view = LibraryView::new();
        view.set_projects(&[p1, p2]);

        // First selection → alpha's first prompt (alphabetical: a1).
        let sp = view.selected_prompt().expect("selection");
        assert_eq!(sp.project, "alpha");
        assert_eq!(sp.slug, "a1");
        assert_eq!(sp.title, "a1");
        assert_eq!(sp.body, "body of a1");

        // Move to last flat row → beta's b1.
        view.move_down();
        view.move_down();
        let sp = view.selected_prompt().expect("selection after move");
        assert_eq!(sp.project, "beta");
        assert_eq!(sp.slug, "b1");
        assert_eq!(sp.title, "b1");
        assert_eq!(sp.body, "body of b1");
    }

    #[test]
    fn selected_prompt_none_when_empty() {
        let view = LibraryView::new();
        assert!(view.selected_prompt().is_none());
    }
}
