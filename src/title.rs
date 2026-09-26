//! Sidebar title derivation from document text.
//!
//! Kept apart from the catalog and the editor: both derive the same title from
//! slightly different views (a [`Doc`] or the editor's cached layouts), so the
//! rules live in one place.

use crate::doc::Doc;

/// Longest title shown in the sidebar (in characters), before truncation.
const TITLE_MAX: usize = 40;

/// A sidebar title derived from a document's first non-empty line.
pub fn derive_title(doc: &dyn Doc) -> String {
    first_title(doc.snapshot().into_iter().map(|snapshot| {
        snapshot
            .runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
    }))
}

/// The first non-empty title among `texts`, trimmed and truncated, or
/// `"Untitled"` when none qualify.
pub fn first_title<S: AsRef<str>>(texts: impl IntoIterator<Item = S>) -> String {
    texts
        .into_iter()
        .find_map(|text| title_from_text(text.as_ref()))
        .unwrap_or_else(|| "Untitled".to_string())
}

/// The first non-empty line of `text`, trimmed and truncated, if any.
pub fn title_from_text(text: &str) -> Option<String> {
    let line = text.lines().next().unwrap_or("").trim();
    (!line.is_empty()).then(|| truncate_title(line))
}

/// Trim a line to [`TITLE_MAX`] characters, adding an ellipsis when cut.
pub fn truncate_title(text: &str) -> String {
    let mut chars = text.chars();
    let truncated: String = chars.by_ref().take(TITLE_MAX).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}
