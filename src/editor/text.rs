use masonry::kurbo::Point;
use masonry::ui_events::keyboard::Code;
use unicode_segmentation::UnicodeSegmentation;

use crate::doc::{BlockKind, Mark, TextRun};

pub(super) fn dist2(a: Point, b: Point) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

pub(super) fn clamp_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(super) fn prev_grapheme(text: &str, index: usize) -> usize {
    let index = clamp_char_boundary(text, index);
    text.grapheme_indices(true)
        .take_while(|(at, _)| *at < index)
        .last()
        .map(|(at, _)| at)
        .unwrap_or(0)
}

pub(super) fn next_grapheme(text: &str, index: usize) -> usize {
    let index = clamp_char_boundary(text, index);
    text.grapheme_indices(true)
        .map(|(at, _)| at)
        .find(|at| *at > index)
        .unwrap_or(text.len())
}

/// Recognize a markdown-style block prefix and its length in bytes.
pub(super) fn block_prefix(text: &str) -> Option<(BlockKind, usize)> {
    const RULES: [(&str, BlockKind); 4] = [
        ("## ", BlockKind::Heading2),
        ("# ", BlockKind::Heading1),
        ("- ", BlockKind::Bullet),
        ("* ", BlockKind::Bullet),
    ];
    RULES
        .iter()
        .find(|(prefix, _)| text.starts_with(prefix))
        .map(|(prefix, kind)| (*kind, prefix.len()))
}

/// The block kind bound to a Cmd/Ctrl+Alt+digit shortcut.
pub(super) fn block_kind_shortcut(code: Code) -> Option<BlockKind> {
    match code {
        Code::Digit1 => Some(BlockKind::Heading1),
        Code::Digit2 => Some(BlockKind::Heading2),
        Code::Digit0 => Some(BlockKind::Paragraph),
        _ => None,
    }
}

/// Whether every character in `[from, to)` carries `mark`.
pub(super) fn range_marked(runs: &[TextRun], from: usize, to: usize, mark: Mark) -> bool {
    let mut offset = 0;
    let mut covered = false;
    for run in runs {
        let start = offset;
        let end = offset + run.text.len();
        offset = end;
        if start.max(from) < end.min(to) {
            let set = match mark {
                Mark::Bold => run.bold,
                Mark::Italic => run.italic,
            };
            if !set {
                return false;
            }
            covered = true;
        }
    }
    covered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_to_char_boundaries() {
        let text = "héllo";
        assert_eq!(clamp_char_boundary(text, 1), 1);
        assert_eq!(clamp_char_boundary(text, 2), 1);
        assert_eq!(clamp_char_boundary(text, 99), text.len());
    }

    #[test]
    fn grapheme_navigation() {
        let text = "héllo ★";
        let star = text.find('★').unwrap();
        assert_eq!(prev_grapheme(text, star), star - 1);
        assert_eq!(next_grapheme(text, star - 1), star);
        assert_eq!(prev_grapheme(text, 0), 0);
        assert_eq!(next_grapheme(text, text.len()), text.len());
    }

    #[test]
    fn emoji_clusters_are_atomic() {
        let family = "a👨‍👩‍👧b";
        let b = family.find('b').unwrap();
        assert_eq!(prev_grapheme(family, b), "a".len());
        assert_eq!(next_grapheme(family, "a".len()), b);
    }

    #[test]
    fn markdown_prefixes_are_recognized() {
        assert_eq!(block_prefix("# "), Some((BlockKind::Heading1, 2)));
        assert_eq!(block_prefix("## "), Some((BlockKind::Heading2, 3)));
        assert_eq!(block_prefix("- "), Some((BlockKind::Bullet, 2)));
        assert_eq!(block_prefix("hello"), None);
    }

    #[test]
    fn range_marked_detects_partial_marks() {
        let runs = vec![
            TextRun {
                text: "bold".into(),
                bold: true,
                italic: false,
            },
            TextRun {
                text: "plain".into(),
                bold: false,
                italic: false,
            },
        ];
        assert!(range_marked(&runs, 0, 4, Mark::Bold));
        assert!(!range_marked(&runs, 0, 9, Mark::Bold));
        assert!(!range_marked(&runs, 4, 9, Mark::Bold));
    }
}
