use super::*;

/// A cached, laid-out block. Rebuilt only when its runs, kind, or the available
/// width changes — so events and paint can both query geometry cheaply.
pub(super) struct BlockLayout {
    pub(super) id: BlockId,
    pub(super) layout: Layout<BrushIndex>,
    pub(super) runs: Vec<TextRun>,
    pub(super) text: String,
    pub(super) kind: BlockKind,
    pub(super) top: f64,
    pub(super) height: f64,
    /// The wrap width this block was shaped with (page width minus indent).
    pub(super) wrap: f32,
}

impl BlockLayout {
    /// A Parley cursor at `offset`, clamped to a UTF-8 char boundary.
    pub(super) fn cursor_at(&self, offset: usize) -> Cursor {
        Cursor::from_byte_index(
            &self.layout,
            clamp_char_boundary(&self.text, offset),
            Affinity::Downstream,
        )
    }
}

pub(super) fn build_block_layout(
    fcx: &mut FontContext,
    lcx: &mut LayoutContext<BrushIndex>,
    id: BlockId,
    runs: Vec<TextRun>,
    kind: BlockKind,
    width: f32,
) -> BlockLayout {
    let text = runs.iter().map(|run| run.text.as_str()).collect::<String>();
    let wrap = (width - indent(kind) as f32).max(1.0);
    let layout = build_layout(fcx, lcx, &runs, kind, wrap);
    let (line_height, _) = block_metrics(kind);
    let height = (layout.height() as f64).max(kind.font_size() as f64 * line_height as f64);
    BlockLayout {
        id,
        layout,
        runs,
        text,
        kind,
        top: 0.0,
        height,
        wrap,
    }
}

/// A Parley ranged builder preloaded with Essor's base font stack and size.
pub(super) fn base_builder<'a>(
    fcx: &'a mut FontContext,
    lcx: &'a mut LayoutContext<BrushIndex>,
    text: &'a str,
    size: f32,
) -> RangedBuilder<'a, BrushIndex> {
    let mut builder = lcx.ranged_builder(fcx, text, 1.0, true);
    builder.push_default(StyleProperty::FontStack(FontStack::Single(
        FontFamily::Generic(GenericFamily::SansSerif),
    )));
    builder.push_default(StyleProperty::FontSize(size));
    builder
}

pub(super) fn build_layout(
    fcx: &mut FontContext,
    lcx: &mut LayoutContext<BrushIndex>,
    runs: &[TextRun],
    kind: BlockKind,
    width: f32,
) -> Layout<BrushIndex> {
    let text = runs.iter().map(|run| run.text.as_str()).collect::<String>();
    let mut builder = base_builder(fcx, lcx, &text, kind.font_size());
    let (line_height, _) = block_metrics(kind);
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(
        line_height,
    )));
    if matches!(kind, BlockKind::Heading1 | BlockKind::Heading2) {
        builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD));
    }

    let mut offset = 0;
    for run in runs {
        let start = offset;
        let end = offset + run.text.len();
        if run.bold {
            builder.push(StyleProperty::FontWeight(FontWeight::BOLD), start..end);
        }
        if run.italic {
            builder.push(StyleProperty::FontStyle(FontStyle::Italic), start..end);
        }
        offset = end;
    }

    let mut layout = builder.build(&text);
    layout.break_all_lines(Some(width));
    layout.align(Some(width), TextAlign::Start, TextAlignOptions::default());
    layout
}

pub(super) fn line_at_y(layout: &Layout<BrushIndex>, y: f32) -> usize {
    let mut last = 0;
    for (index, line) in layout.lines().enumerate() {
        last = index;
        if y < line.metrics().max_coord {
            return index;
        }
    }
    last
}

pub(super) fn line_center(layout: &Layout<BrushIndex>, line: usize) -> f32 {
    layout
        .get(line)
        .map(|line| {
            let metrics = line.metrics();
            // Vertical center of the em box, which tracks the glyphs rather than
            // the (variable) line box height.
            metrics.baseline - (metrics.ascent - metrics.descent) * 0.5
        })
        .unwrap_or(0.0)
}

/// A width change that is waiting for a live resize to settle before the
/// document is re-shaped. Re-shaping on every resize event is far too
/// expensive, so a change is deferred until the width has held still for
/// [`REFLOW_DELAY`].
pub(super) struct Reflow {
    pending: Option<f32>,
    changed_at: Instant,
    force: bool,
}

impl Reflow {
    pub(super) fn new() -> Self {
        Self {
            pending: None,
            changed_at: Instant::now(),
            force: false,
        }
    }

    /// Remember a target width, restarting the settle window when it changes.
    pub(super) fn defer(&mut self, width: f32) {
        if self.pending != Some(width) {
            self.pending = Some(width);
            self.changed_at = Instant::now();
        }
    }

    /// Whether the deferred width has settled and differs from `current`.
    pub(super) fn ready(&self, current: f32) -> bool {
        self.pending.is_some_and(|width| {
            self.changed_at.elapsed() >= REFLOW_DELAY && (current - width).abs() > f32::EPSILON
        })
    }

    /// Mark a settled reflow for application on the next layout pass.
    pub(super) fn request(&mut self) {
        self.force = true;
    }

    pub(super) fn forced(&self) -> bool {
        self.force
    }

    /// Clear after a re-shape has been performed.
    pub(super) fn finish(&mut self) {
        self.pending = None;
        self.force = false;
    }

    pub(super) fn reset(&mut self) {
        self.finish();
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> Option<f32> {
        self.pending
    }
}

impl Editor {
    /// Left edge of a block's text (page margin plus any bullet indent).
    pub(super) fn block_text_x(&self, block: &BlockLayout) -> f64 {
        self.page_left + indent(block.kind)
    }

    // --- layout cache -----------------------------------------------------

    pub(super) fn ensure_layouts(
        &mut self,
        fcx: &mut FontContext,
        lcx: &mut LayoutContext<BrushIndex>,
        width: f32,
    ) {
        // Shape at whole logical pixels: sub-pixel width changes (which a live
        // resize can produce) would otherwise force a full re-shape.
        let width = width.round();

        // Edits always set `layouts_dirty`, and only the editor mutates the
        // document, so when nothing is dirty and the width is unchanged the
        // cached layouts are still valid. Checking that first avoids snapshotting
        // every block on the many no-op layout passes a frame can trigger.
        let width_changed = self.layout_width != width;
        let added_or_removed = self.layouts.len() != self.doc.len();
        let immediate = self.layouts_dirty
            || added_or_removed
            || self.reflow.forced()
            || self.layouts.is_empty();

        // Re-shaping every block on each resize event is far too expensive, so
        // keep the previous layouts and defer until the width stops changing.
        if width_changed && !immediate {
            self.reflow.defer(width);
            return;
        }
        if !width_changed && !immediate {
            return;
        }

        self.reflow.finish();

        let snapshot = self.doc.snapshot();
        // A remote edit can insert and remove a block at once, keeping the
        // length the same while shifting every id after it. Compare the id
        // sequence, not just the length, before reusing cached layouts.
        let ids_changed = self.layouts.len() != snapshot.len()
            || self
                .layouts
                .iter()
                .zip(&snapshot)
                .any(|(cached, data)| cached.id != data.id);

        let old_index = std::mem::take(&mut self.block_index);

        if width_changed || ids_changed {
            self.layouts = snapshot
                .into_iter()
                .map(|data| build_block_layout(fcx, lcx, data.id, data.runs, data.kind, width))
                .collect();
        } else {
            // Rebuild only the blocks whose runs or kind actually changed; reuse
            // the (expensive) Parley layouts of untouched blocks.
            for (index, data) in snapshot.into_iter().enumerate() {
                let cached = &self.layouts[index];
                if cached.runs != data.runs || cached.kind != data.kind {
                    self.layouts[index] =
                        build_block_layout(fcx, lcx, data.id, data.runs, data.kind, width);
                }
            }
        }
        self.layout_width = width;
        self.layouts_dirty = false;
        self.block_index = self
            .layouts
            .iter()
            .enumerate()
            .map(|(index, layout)| (layout.id, index))
            .collect();

        let mut y = PAGE_TOP;
        for (index, layout) in self.layouts.iter_mut().enumerate() {
            let (_, space_above) = block_metrics(layout.kind);
            if index > 0 {
                y += space_above;
            }
            layout.top = y;
            y += layout.height;
        }
        self.content_height = y + PAGE_BOTTOM;

        self.selection
            .reconcile(&old_index, &self.layouts, &self.block_index);
    }

    /// Re-shape the document after an edit. The *cached* `layout_width` is used
    /// deliberately: during a live resize a new width is pending, and shaping to
    /// it here would defeat the deferred reflow, so edits keep the old wrap until
    /// the resize settles.
    pub(super) fn refresh_layouts(&mut self, ctx: &mut EventCtx<'_>) {
        if self.layout_width <= 0.0 {
            return;
        }
        let width = self.layout_width;
        let (fcx, lcx) = ctx.text_contexts();
        self.ensure_layouts(fcx, lcx, width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflow_defers_until_the_width_settles() {
        let mut reflow = Reflow::new();
        reflow.defer(720.0);

        // Deferred for a moment after the change...
        assert!(!reflow.ready(800.0));
        // ...and never applies when the width did not actually change.
        std::thread::sleep(REFLOW_DELAY + std::time::Duration::from_millis(20));
        assert!(reflow.ready(800.0));
        assert!(!reflow.ready(720.0));

        reflow.request();
        assert!(reflow.forced());
        reflow.finish();
        assert!(!reflow.forced());
        assert!(reflow.pending().is_none());
    }
}
