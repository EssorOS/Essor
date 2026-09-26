use super::*;

/// How a keyboard event was handled.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyHandled {
    /// Not handled; leave the event for the framework.
    No,
    /// Handled; content may have changed, so refresh the slash query too.
    Edited,
    /// Handled without any possible slash-query change.
    Consumed,
}

impl Editor {
    // --- editing operations ----------------------------------------------

    /// The block at the caret, and its current index, lazily seeding one when
    /// the document is still empty.
    ///
    /// Documents materialised from a peer's list start with no blocks and receive
    /// their content over sync. If the user edits before that content arrives we
    /// create a block so the page stays usable. The peer's own seed block may
    /// later merge alongside it, which is harmless: both are preserved.
    fn editable_block(&mut self) -> Option<(BlockId, usize)> {
        if let Some(index) = self.index_of(self.selection.focus.block)
            && let Some(layout) = self.layouts.get(index)
        {
            return Some((layout.id, index));
        }
        if self.doc.is_empty() {
            let block = self.doc.insert_block(0);
            self.set_caret(Position::new(block, 0));
            self.layouts_dirty = true;
            return Some((block, 0));
        }
        None
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        self.delete_selection();
        let pos = self.selection.focus;
        let Some((block, _)) = self.editable_block() else {
            return;
        };
        let mut value = self.doc.text(block);
        let at = clamp_char_boundary(&value, pos.offset);
        value.insert_str(at, text);
        self.doc.set_text(block, &value);
        self.set_caret(Position::new(block, at + text.len()));
        self.layouts_dirty = true;

        // Markdown-style block rules: "# ", "## ", "- " on a paragraph.
        if self.doc.kind(block) == BlockKind::Paragraph
            && let Some((kind, marker_len)) = block_prefix(&value)
        {
            self.doc.set_kind(block, kind);
            self.doc.set_text(block, &value[marker_len..]);
            self.set_caret(Position::new(block, 0));
        } else if value == "/" && self.doc.kind(block) == BlockKind::Paragraph {
            self.open_slash_menu();
        }
    }

    pub(super) fn toggle_mark(&mut self, mark: Mark) {
        if self.collapsed() {
            return;
        }
        let (start, end) = self.selection();
        let (Some(start_index), Some(end_index)) =
            (self.index_of(start.block), self.index_of(end.block))
        else {
            return;
        };
        for index in start_index..=end_index {
            let Some((from, to)) = self
                .selection_in_block(index)
                .filter(|(from, to)| from < to)
            else {
                continue;
            };
            let Some(block) = self.block(index) else {
                continue;
            };
            let runs = self.doc.runs(block);
            let on = !range_marked(&runs, from, to, mark);
            self.doc.mark(block, from, to, mark, on);
        }
        self.layouts_dirty = true;
    }

    pub(super) fn set_selected_kind(&mut self, kind: BlockKind) {
        let (start, end) = self.selection();
        let (Some(start_index), Some(end_index)) =
            (self.index_of(start.block), self.index_of(end.block))
        else {
            return;
        };
        for index in start_index..=end_index {
            if let Some(block) = self.block(index) {
                self.doc.set_kind(block, kind);
            }
        }
        self.layouts_dirty = true;
    }

    pub(super) fn split(&mut self) {
        self.delete_selection();
        let pos = self.selection.focus;
        let Some((block, index)) = self.editable_block() else {
            return;
        };
        let value = self.doc.text(block);
        let at = clamp_char_boundary(&value, pos.offset);
        let right = value[at..].to_string();
        self.doc.set_text(block, &value[..at]);
        let next = self.doc.insert_block(index + 1);
        self.doc.set_text(next, &right);
        self.set_caret(Position::new(next, 0));
        self.layouts_dirty = true;
    }

    pub(super) fn backspace(&mut self) {
        if !self.collapsed() {
            self.delete_selection();
            return;
        }
        let pos = self.selection.focus;
        let Some(index) = self.index_of(pos.block) else {
            return;
        };
        let Some(block) = self.block(index) else {
            return;
        };
        let value = self.doc.text(block);
        if pos.offset > 0 {
            let at = clamp_char_boundary(&value, pos.offset);
            let prev = prev_grapheme(&value, at);
            let mut merged = value;
            merged.replace_range(prev..at, "");
            self.doc.set_text(block, &merged);
            self.set_caret(Position::new(block, prev));
        } else if index > 0 {
            let Some(previous) = self.block(index - 1) else {
                return;
            };
            let before = self.doc.text(previous);
            let at = before.len();
            self.doc.set_text(previous, &format!("{before}{value}"));
            self.doc.remove_block(block);
            self.set_caret(Position::new(previous, at));
        }
        self.layouts_dirty = true;
    }

    pub(super) fn delete_forward(&mut self) {
        if !self.collapsed() {
            self.delete_selection();
            return;
        }
        let pos = self.selection.focus;
        let Some(index) = self.index_of(pos.block) else {
            return;
        };
        let Some(block) = self.block(index) else {
            return;
        };
        let value = self.doc.text(block);
        if pos.offset < value.len() {
            let at = clamp_char_boundary(&value, pos.offset);
            let next = next_grapheme(&value, at);
            let mut merged = value;
            merged.replace_range(at..next, "");
            self.doc.set_text(block, &merged);
        } else if index + 1 < self.doc.len() {
            let Some(following) = self.block(index + 1) else {
                return;
            };
            let after = self.doc.text(following);
            self.doc.set_text(block, &format!("{value}{after}"));
            self.doc.remove_block(following);
        }
        self.layouts_dirty = true;
    }

    pub(super) fn delete_selection(&mut self) {
        if self.collapsed() {
            return;
        }
        let (start, end) = self.selection();
        let (Some(start_index), Some(end_index)) =
            (self.index_of(start.block), self.index_of(end.block))
        else {
            return;
        };
        let (Some(start_block), Some(end_block)) = (self.block(start_index), self.block(end_index))
        else {
            return;
        };
        let start_text = self.doc.text(start_block);
        let end_text = self.doc.text(end_block);
        let start_at = clamp_char_boundary(&start_text, start.offset);
        let end_at = clamp_char_boundary(&end_text, end.offset);

        if start_index == end_index {
            let mut merged = start_text;
            merged.replace_range(start_at..end_at, "");
            self.doc.set_text(start_block, &merged);
        } else {
            let merged = format!("{}{}", &start_text[..start_at], &end_text[end_at..]);
            // Collect the doomed ids up front: removing one block shifts the
            // positions of the rest, so addressing by index would miss some.
            let doomed: Vec<BlockId> = self.layouts[start_index + 1..=end_index]
                .iter()
                .map(|layout| layout.id)
                .collect();
            for id in doomed {
                self.doc.remove_block(id);
            }
            self.doc.set_text(start_block, &merged);
        }

        self.set_caret(Position::new(start_block, start_at));
        self.layouts_dirty = true;
    }

    // --- clipboard & history ---------------------------------------------

    pub(super) fn selected_text(&self) -> String {
        if self.collapsed() {
            return String::new();
        }
        let (start, end) = self.selection();
        let (Some(start_index), Some(end_index)) =
            (self.index_of(start.block), self.index_of(end.block))
        else {
            return String::new();
        };
        let mut parts = Vec::new();
        for index in start_index..=end_index {
            let Some((from, to)) = self.selection_in_block(index) else {
                continue;
            };
            if let Some(block) = self.layouts.get(index) {
                parts.push(block.text[from..to].to_string());
            }
        }
        parts.join("\n")
    }

    pub(super) fn copy(&self, ctx: &mut EventCtx<'_>) {
        let text = self.selected_text();
        if !text.is_empty() {
            ctx.set_clipboard(text);
        }
    }

    pub(super) fn cut(&mut self, ctx: &mut EventCtx<'_>) {
        let text = self.selected_text();
        if !text.is_empty() {
            ctx.set_clipboard(text);
            self.delete_selection();
        }
    }

    pub(super) fn select_all(&mut self) {
        let (Some(first), Some(last)) = (self.layouts.first(), self.layouts.last()) else {
            return;
        };
        self.selection.anchor = Position::new(first.id, 0);
        self.selection.focus = Position::new(last.id, last.text.len());
        self.selection.preferred_x = None;
    }

    pub(super) fn undo(&mut self) {
        if self.doc.undo() {
            self.layouts_dirty = true;
        }
    }

    pub(super) fn redo(&mut self) {
        if self.doc.redo() {
            self.layouts_dirty = true;
        }
    }

    // --- text event handling ---------------------------------------------

    /// Handle an IME or clipboard text event. Returns whether it was handled.
    pub(super) fn handle_ime_or_clipboard(&mut self, event: &TextEvent) -> bool {
        match event {
            TextEvent::Ime(Ime::Commit(text)) => {
                self.preedit.clear();
                self.insert_text(text);
            }
            TextEvent::Ime(Ime::Preedit(text, _)) => {
                self.preedit.clone_from(text);
            }
            TextEvent::Ime(Ime::Disabled) => self.preedit.clear(),
            TextEvent::Ime(Ime::Enabled) => {}
            TextEvent::ClipboardPaste(text) => self.insert_text(text),
            _ => return false,
        }
        true
    }

    /// Apply a keyboard shortcut or editing command. Returns how the event was
    /// handled (including whether the slash query may need refreshing).
    pub(super) fn handle_key(&mut self, ctx: &mut EventCtx<'_>, key: &KeyboardEvent) -> KeyHandled {
        let shift = key.modifiers.shift();
        let word = key.modifiers.alt() || key.modifiers.ctrl();
        let action_mod = if cfg!(target_os = "macos") {
            key.modifiers.meta()
        } else {
            key.modifiers.ctrl()
        };

        // Cmd/Ctrl+Alt+1/2/0 set the block kind on the selection.
        if action_mod
            && key.modifiers.alt()
            && let Some(kind) = block_kind_shortcut(key.code)
        {
            self.set_selected_kind(kind);
            return KeyHandled::Consumed;
        }

        if action_mod && let Key::Character(c) = &key.key {
            return self.handle_command(&c.to_ascii_lowercase(), shift, ctx);
        }

        match &key.key {
            Key::Character(text) => self.insert_text(text),
            Key::Named(NamedKey::Enter) => self.split(),
            Key::Named(NamedKey::Backspace) => self.backspace(),
            Key::Named(NamedKey::Delete) => self.delete_forward(),
            Key::Named(NamedKey::ArrowLeft) => self.move_horizontal(-1, shift, word),
            Key::Named(NamedKey::ArrowRight) => self.move_horizontal(1, shift, word),
            Key::Named(NamedKey::ArrowUp) => self.move_vertical(-1, shift),
            Key::Named(NamedKey::ArrowDown) => self.move_vertical(1, shift),
            Key::Named(NamedKey::Home) => self.move_line_edge(false, shift),
            Key::Named(NamedKey::End) => self.move_line_edge(true, shift),
            _ => return KeyHandled::No,
        }
        KeyHandled::Edited
    }

    /// Apply an action-modifier command character. Returns `No` for unknown ones.
    pub(super) fn handle_command(
        &mut self,
        command: &str,
        shift: bool,
        ctx: &mut EventCtx<'_>,
    ) -> KeyHandled {
        match command {
            "c" => self.copy(ctx),
            "x" => self.cut(ctx),
            "a" => self.select_all(),
            "b" => self.toggle_mark(Mark::Bold),
            "i" => self.toggle_mark(Mark::Italic),
            "z" if shift => self.redo(),
            "z" => self.undo(),
            _ => return KeyHandled::No,
        }
        KeyHandled::Edited
    }
}
