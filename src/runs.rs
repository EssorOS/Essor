//! Pure helpers for a block's run list.
//!
//! A block's content is a `Vec<TextRun>`; edits splice it at a byte offset,
//! preserving the marks of untouched text and inheriting the mark at an edit
//! boundary. These functions touch no store, so they are easy to reason about
//! and test in isolation.

use crate::model::TextRun;

/// Collect the runs' text.
pub(crate) fn runs_text(runs: &[TextRun]) -> String {
    runs.iter().map(|run| run.text.as_str()).collect()
}

/// Merge adjacent runs that share marks.
pub(crate) fn coalesce(runs: Vec<TextRun>) -> Vec<TextRun> {
    let mut out: Vec<TextRun> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.bold == run.bold && last.italic == run.italic => {
                last.text.push_str(&run.text);
            }
            _ => out.push(run),
        }
    }
    out
}

/// Split `runs` at byte offset `at` into (left, right); `at` must be on a char
/// boundary within the runs' text.
pub(crate) fn split_runs(runs: &[TextRun], at: usize) -> (Vec<TextRun>, Vec<TextRun>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut offset = 0;
    let mut split = false;
    for run in runs {
        let end = offset + run.text.len();
        if split {
            right.push(run.clone());
        } else if at <= offset {
            split = true;
            right.push(run.clone());
        } else if at >= end {
            left.push(run.clone());
        } else {
            let local = at - offset;
            left.push(TextRun {
                text: run.text[..local].to_string(),
                bold: run.bold,
                italic: run.italic,
            });
            right.push(TextRun {
                text: run.text[local..].to_string(),
                bold: run.bold,
                italic: run.italic,
            });
            split = true;
        }
        offset = end;
    }
    (left, right)
}

/// The marks of the character beginning at byte `at`, or of the last character
/// when `at` is at the end. This is what an insertion at `at` inherits.
pub(crate) fn marks_at(runs: &[TextRun], at: usize) -> (bool, bool) {
    let mut offset = 0;
    let mut last = (false, false);
    for run in runs {
        let end = offset + run.text.len();
        last = (run.bold, run.italic);
        if at < end {
            return last;
        }
        offset = end;
    }
    last
}

/// The shared prefix and suffix byte lengths of `old` and `new`, trimmed back to
/// char boundaries in both strings.
pub(crate) fn common_edges(old: &str, new: &str) -> (usize, usize) {
    let max = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max && old.as_bytes()[prefix] == new.as_bytes()[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let mut suffix = 0;
    let max_suffix = max - prefix;
    while suffix < max_suffix
        && old.as_bytes()[old.len() - 1 - suffix] == new.as_bytes()[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && !(old.is_char_boundary(old.len() - suffix) && new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    (prefix, suffix)
}

/// Clamp a byte offset to `text`'s length and back to a char boundary.
pub(crate) fn clamp_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, bold: bool, italic: bool) -> TextRun {
        TextRun {
            text: text.to_string(),
            bold,
            italic,
        }
    }

    #[test]
    fn splitting_inside_a_run_preserves_marks_on_both_sides() {
        let runs = vec![run("hello", true, false), run(" world", false, false)];
        let (left, right) = split_runs(&runs, 3);
        assert_eq!(runs_text(&left), "hel");
        assert!(left[0].bold);
        assert_eq!(runs_text(&right), "lo world");
        assert!(right[0].bold, "the tail of the split run keeps the mark");
        assert!(!right[1].bold);
    }

    #[test]
    fn coalescing_merges_adjacent_runs_with_equal_marks() {
        let runs = vec![run("a", true, false), run("b", true, false), run("c", false, false)];
        let merged = coalesce(runs);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "ab");
    }

    #[test]
    fn common_edges_trims_back_to_char_boundaries() {
        // The byte in the middle of "é" differs between the two strings.
        let (prefix, suffix) = common_edges("café", "café!");
        assert_eq!(&"café"[..prefix], "café");
        assert_eq!(suffix, 0);
    }
}
