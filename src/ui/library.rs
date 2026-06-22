use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

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
    /// User pressed `n`/`a` — typing a title for a new prompt.
    NewPrompt,
}

// ---------------------------------------------------------------------------
// LibraryView
// ---------------------------------------------------------------------------

pub struct LibraryView {
    store: PromptStore,
    /// All prompts loaded from disk.
    all_prompts: Vec<Prompt>,
    /// Indices into `all_prompts` that pass the current filter.
    filtered: Vec<usize>,
    /// Ratatui list selection state (index into `filtered`).
    list_state: ListState,
    pub mode: LibraryMode,
    /// Current text typed in filter or new-prompt input box.
    pub input: String,
    /// Status message shown at the bottom (clears on next keypress).
    pub status: Option<String>,
}

impl LibraryView {
    pub fn new(store: PromptStore) -> Self {
        let all_prompts = store.load_all().unwrap_or_else(|e| {
            tracing::warn!("library: could not load prompts: {e}");
            vec![]
        });
        let filtered: Vec<usize> = (0..all_prompts.len()).collect();
        let mut list_state = ListState::default();
        if !filtered.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            store,
            all_prompts,
            filtered,
            list_state,
            mode: LibraryMode::Normal,
            input: String::new(),
            status: None,
        }
    }

    // -----------------------------------------------------------------------
    // Reload
    // -----------------------------------------------------------------------

    fn reload(&mut self) {
        self.all_prompts = self.store.load_all().unwrap_or_else(|e| {
            tracing::warn!("library: reload failed: {e}");
            vec![]
        });
        self.apply_filter();
    }

    /// All library prompts as `(slug, title, body)` tuples, used to build the
    /// new-worktree modal choices.  Order matches the loaded prompt order.
    pub fn all_prompt_choices(&self) -> Vec<(String, String, String)> {
        self.all_prompts
            .iter()
            .map(|p| (p.slug.clone(), p.title.clone(), p.body.clone()))
            .collect()
    }

    /// Filesystem path of the currently selected prompt, if any.
    pub fn selected_prompt_path(&self) -> Option<std::path::PathBuf> {
        let filtered_idx = self.list_state.selected()?;
        let prompt_idx = *self.filtered.get(filtered_idx)?;
        let slug = &self.all_prompts.get(prompt_idx)?.slug;
        Some(self.store.path_for(slug))
    }

    /// Reload from disk after an external edit, keeping the selection pinned to
    /// the same prompt slug when it still exists.
    pub fn reload_keep_selection(&mut self) {
        let prev_slug = self
            .list_state
            .selected()
            .and_then(|fi| self.filtered.get(fi))
            .and_then(|&pi| self.all_prompts.get(pi))
            .map(|p| p.slug.clone());

        self.reload();

        if let Some(slug) = prev_slug {
            if let Some(pi) = self.all_prompts.iter().position(|p| p.slug == slug) {
                if let Some(fi) = self.filtered.iter().position(|&i| i == pi) {
                    self.list_state.select(Some(fi));
                }
            }
        }
    }

    fn apply_filter(&mut self) {
        let query = if self.mode == LibraryMode::Filter {
            self.input.as_str()
        } else {
            ""
        };
        self.filtered = PromptStore::search(&self.all_prompts, query)
            .into_iter()
            .map(|p| {
                self.all_prompts
                    .iter()
                    .position(|a| std::ptr::eq(a, p))
                    .unwrap_or(0)
            })
            .collect();

        // Clamp selection.
        let sel = self.list_state.selected().unwrap_or(0);
        if self.filtered.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state
                .select(Some(sel.min(self.filtered.len() - 1)));
        }
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let next = (i + 1).min(self.filtered.len() - 1);
        self.list_state.select(Some(next));
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let prev = i.saturating_sub(1);
        self.list_state.select(Some(prev));
    }

    // -----------------------------------------------------------------------
    // Filter mode
    // -----------------------------------------------------------------------

    pub fn enter_filter(&mut self) {
        self.mode = LibraryMode::Filter;
        self.input.clear();
        self.status = None;
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
    // New prompt mode
    // -----------------------------------------------------------------------

    pub fn enter_new_prompt(&mut self) {
        self.mode = LibraryMode::NewPrompt;
        self.input.clear();
        self.status = None;
    }

    pub fn new_prompt_push(&mut self, ch: char) {
        self.input.push(ch);
    }

    pub fn new_prompt_pop(&mut self) {
        self.input.pop();
    }

    /// Confirm the new prompt: slugify the title, create an empty Prompt, save
    /// it, and reload the list.  Returns an error string on failure.
    pub fn confirm_new_prompt(&mut self) -> Result<(), String> {
        let title = self.input.trim().to_string();
        if title.is_empty() {
            self.mode = LibraryMode::Normal;
            self.input.clear();
            return Ok(());
        }
        let slug = slugify(&title);
        if slug.is_empty() {
            return Err("title produced an empty slug — use alphanumeric characters".to_string());
        }

        let prompt = Prompt {
            slug: slug.clone(),
            title: title.clone(),
            tags: vec![],
            vars: vec![],
            body: String::new(),
        };

        self.store.save(&prompt).map_err(|e| e.to_string())?;
        self.mode = LibraryMode::Normal;
        self.input.clear();
        self.reload();

        // Select the newly created prompt.
        if let Some(pos) = self.all_prompts.iter().position(|p| p.slug == slug) {
            if let Some(filtered_pos) = self.filtered.iter().position(|&i| i == pos) {
                self.list_state.select(Some(filtered_pos));
            }
        }

        self.status = Some(format!("created \"{title}\""));
        Ok(())
    }

    pub fn cancel_input(&mut self) {
        self.mode = LibraryMode::Normal;
        self.input.clear();
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

        // ---- Build list items ----
        let items: Vec<ListItem> = self
            .filtered
            .iter()
            .map(|&idx| {
                let p = &self.all_prompts[idx];
                let tags_str = if p.tags.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", p.tags.join(", "))
                };
                ListItem::new(Line::from(vec![
                    Span::styled(p.title.clone(), Style::default().fg(Color::White)),
                    Span::styled(tags_str, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

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

        let list = List::new(items)
            .block(list_block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.list_state);

        // ---- Bottom bar ----
        let bottom_text = match &self.mode {
            LibraryMode::Normal => {
                let status = self
                    .status
                    .as_deref()
                    .unwrap_or("j/k: move  /: filter  n: new  e: edit  q: quit");
                status.to_string()
            }
            LibraryMode::Filter => {
                format!("filter: {}█  Esc: clear", self.input)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(slugs: &[&str]) -> (tempfile::TempDir, PromptStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PromptStore::new(dir.path());
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
        (dir, store)
    }

    #[test]
    fn selected_prompt_path_points_at_selection_file() {
        let (_dir, store) = store_with(&["alpha"]);
        let expected = store.path_for("alpha");
        let view = LibraryView::new(store);
        assert_eq!(view.selected_prompt_path(), Some(expected));
    }

    #[test]
    fn selected_prompt_path_none_when_empty() {
        let (_dir, store) = store_with(&[]);
        let view = LibraryView::new(store);
        assert_eq!(view.selected_prompt_path(), None);
    }

    #[test]
    fn reload_keep_selection_stays_on_same_slug() {
        let (_dir, store) = store_with(&["alpha", "beta", "gamma"]);
        let mut view = LibraryView::new(store);
        // Pin selection to whatever slug is currently selected.
        let before = view.selected_prompt_path().expect("a selection");
        view.reload_keep_selection();
        assert_eq!(view.selected_prompt_path(), Some(before));
    }
}
