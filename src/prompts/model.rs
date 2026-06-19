/// A single prompt in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// URL-safe identifier derived from the title (e.g. `"my-prompt"`).
    pub slug: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Template variable names referenced in the body (e.g. `["repo", "branch"]`).
    pub vars: Vec<String>,
    pub body: String,
}

/// Convert an arbitrary title string to a lowercase hyphen-separated slug.
///
/// Rules:
/// 1. Lowercase everything.
/// 2. Replace spaces with hyphens.
/// 3. Strip characters that are not alphanumeric or hyphens.
/// 4. Collapse consecutive hyphens into one.
/// 5. Trim leading/trailing hyphens.
pub fn slugify(title: &str) -> String {
    let lowered = title.to_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut prev_hyphen = false;

    for ch in lowered.chars() {
        if ch == ' ' || ch == '-' {
            if !prev_hyphen && !slug.is_empty() {
                slug.push('-');
                prev_hyphen = true;
            }
        } else if ch.is_alphanumeric() {
            slug.push(ch);
            prev_hyphen = false;
        }
        // All other characters are dropped.
    }

    // Trim trailing hyphens.
    let trimmed = slug.trim_end_matches('-');
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn slugify_strips_special_chars() {
        assert_eq!(slugify("Fix: bug #42!"), "fix-bug-42");
    }

    #[test]
    fn slugify_collapses_hyphens() {
        assert_eq!(slugify("a  b"), "a-b");
    }

    #[test]
    fn slugify_already_clean() {
        assert_eq!(slugify("my-prompt"), "my-prompt");
    }

    #[test]
    fn slugify_empty() {
        assert_eq!(slugify(""), "");
    }
}
