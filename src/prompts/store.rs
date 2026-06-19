use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::model::{slugify, Prompt};

// ---------------------------------------------------------------------------
// Internal TOML frontmatter representation
// ---------------------------------------------------------------------------

/// Serialisable form that lives between the `+++` fences.
#[derive(Debug, Serialize, Deserialize)]
struct Frontmatter {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    vars: Vec<String>,
}

// ---------------------------------------------------------------------------
// PromptStore
// ---------------------------------------------------------------------------

/// Flat-file prompt store.  Each prompt is stored as `<dir>/<slug>.md` with
/// TOML frontmatter fenced by `+++` lines, followed by a Markdown body.
///
/// File format:
/// ```text
/// +++
/// title = "My Prompt"
/// tags  = ["rust", "refactor"]
/// vars  = ["repo", "branch"]
/// +++
///
/// Body text here.
/// ```
pub struct PromptStore {
    pub dir: PathBuf,
}

impl PromptStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Load every `.md` file in `self.dir`.  Malformed files are skipped with
    /// a warning log; they never cause `load_all` to fail.
    pub fn load_all(&self) -> Result<Vec<Prompt>> {
        let mut prompts = Vec::new();

        let read_dir = std::fs::read_dir(&self.dir)
            .with_context(|| format!("cannot read prompt dir {:?}", self.dir))?;

        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("skipping unreadable dir entry: {e}");
                    continue;
                }
            };

            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("skipping {:?}: cannot read: {e}", path);
                    continue;
                }
            };

            match Self::parse(&content) {
                Ok(mut prompt) => {
                    // Derive slug from the filename stem so it always stays in sync.
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        prompt.slug = stem.to_string();
                    }
                    prompts.push(prompt);
                }
                Err(e) => {
                    tracing::warn!("skipping {:?}: parse error: {e}", path);
                }
            }
        }

        prompts.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(prompts)
    }

    /// Write `prompt` to `<dir>/<slug>.md`, creating the dir if needed.
    pub fn save(&self, prompt: &Prompt) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create prompt dir {:?}", self.dir))?;

        let path = self.dir.join(format!("{}.md", prompt.slug));
        let content = Self::serialize(prompt);
        std::fs::write(&path, content).with_context(|| format!("cannot write {:?}", path))?;
        Ok(())
    }

    /// Parse a full file content string into a `Prompt`.
    pub fn parse(content: &str) -> Result<Prompt> {
        // Expect the file to open with `+++\n`.
        let rest = content
            .strip_prefix("+++\n")
            .or_else(|| content.strip_prefix("+++\r\n"))
            .context("missing opening +++ fence")?;

        // Find the closing `+++`.
        let close = rest.find("\n+++").context("missing closing +++ fence")?;

        let frontmatter_str = &rest[..close];
        let after_fence = &rest[close + 4..]; // skip "\n+++"

        // Body starts after the optional newline following `+++`.
        let body = after_fence
            .strip_prefix('\n')
            .or_else(|| after_fence.strip_prefix("\r\n"))
            .unwrap_or(after_fence)
            .to_string();

        let fm: Frontmatter =
            toml::from_str(frontmatter_str).context("invalid TOML frontmatter")?;

        let slug = slugify(&fm.title);

        Ok(Prompt {
            slug,
            title: fm.title,
            tags: fm.tags,
            vars: fm.vars,
            body,
        })
    }

    /// Serialise a `Prompt` into the on-disk file format.
    pub fn serialize(prompt: &Prompt) -> String {
        let fm = Frontmatter {
            title: prompt.title.clone(),
            tags: prompt.tags.clone(),
            vars: prompt.vars.clone(),
        };
        let toml_str = toml::to_string(&fm).expect("Frontmatter serialisation cannot fail");
        format!("+++\n{}+++\n{}", toml_str, prompt.body)
    }

    /// Filter a slice of prompts by a case-insensitive substring query that
    /// matches against title or any tag.
    pub fn search<'a>(prompts: &'a [Prompt], query: &str) -> Vec<&'a Prompt> {
        if query.is_empty() {
            return prompts.iter().collect();
        }
        let lower = query.to_lowercase();
        prompts
            .iter()
            .filter(|p| {
                p.title.to_lowercase().contains(&lower)
                    || p.tags.iter().any(|t| t.to_lowercase().contains(&lower))
            })
            .collect()
    }

    /// Convenience: load all, then apply a search filter.
    #[allow(dead_code)]
    pub fn load_filtered(&self, query: &str) -> Result<Vec<Prompt>> {
        let all = self.load_all()?;
        if query.is_empty() {
            return Ok(all);
        }
        Ok(Self::search(&all, query).into_iter().cloned().collect())
    }

    /// Return the file path for a given slug.
    #[allow(dead_code)]
    pub fn path_for(&self, slug: &str) -> PathBuf {
        self.dir.join(format!("{slug}.md"))
    }
}

/// Check whether a prompt file exists for `slug`.
#[allow(dead_code)]
pub fn prompt_exists(dir: &Path, slug: &str) -> bool {
    dir.join(format!("{slug}.md")).exists()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_prompt() -> Prompt {
        Prompt {
            slug: "fix-the-bug".to_string(),
            title: "Fix the Bug".to_string(),
            tags: vec!["rust".to_string(), "debug".to_string()],
            vars: vec!["repo".to_string()],
            body: "Please fix the bug in {{repo}}.\n".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_serialize_parse() {
        let original = sample_prompt();
        let serialized = PromptStore::serialize(&original);
        let parsed = PromptStore::parse(&serialized).expect("parse failed");

        // slug is re-derived from title during parse; it must match
        assert_eq!(parsed.slug, original.slug);
        assert_eq!(parsed.title, original.title);
        assert_eq!(parsed.tags, original.tags);
        assert_eq!(parsed.vars, original.vars);
        assert_eq!(parsed.body, original.body);
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PromptStore::new(dir.path());
        let original = sample_prompt();

        store.save(&original).expect("save");

        let loaded = store.load_all().expect("load_all");
        assert_eq!(loaded.len(), 1);
        let got = &loaded[0];

        assert_eq!(got.slug, original.slug);
        assert_eq!(got.title, original.title);
        assert_eq!(got.tags, original.tags);
        assert_eq!(got.vars, original.vars);
        assert_eq!(got.body, original.body);
    }

    // -----------------------------------------------------------------------
    // Malformed file handling
    // -----------------------------------------------------------------------

    #[test]
    fn malformed_file_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Write a bad file.
        std::fs::write(dir.path().join("bad.md"), "not a valid format").unwrap();
        // Write a good file alongside it.
        let store = PromptStore::new(dir.path());
        store.save(&sample_prompt()).expect("save");

        let loaded = store.load_all().expect("load_all should not fail");
        // Only the good prompt is returned.
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].slug, "fix-the-bug");
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    #[test]
    fn search_matches_title_case_insensitive() {
        let prompts = vec![
            Prompt {
                slug: "foo".into(),
                title: "Fix the Bug".into(),
                tags: vec![],
                vars: vec![],
                body: String::new(),
            },
            Prompt {
                slug: "bar".into(),
                title: "Add Feature".into(),
                tags: vec![],
                vars: vec![],
                body: String::new(),
            },
        ];

        let results = PromptStore::search(&prompts, "FIX");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "foo");
    }

    #[test]
    fn search_matches_tags_case_insensitive() {
        let prompts = vec![
            Prompt {
                slug: "a".into(),
                title: "Some Prompt".into(),
                tags: vec!["Rust".into(), "TUI".into()],
                vars: vec![],
                body: String::new(),
            },
            Prompt {
                slug: "b".into(),
                title: "Other Prompt".into(),
                tags: vec!["python".into()],
                vars: vec![],
                body: String::new(),
            },
        ];

        let results = PromptStore::search(&prompts, "rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].slug, "a");
    }

    #[test]
    fn search_empty_query_returns_all() {
        let prompts = vec![sample_prompt(), sample_prompt()];
        let results = PromptStore::search(&prompts, "");
        assert_eq!(results.len(), 2);
    }
}
