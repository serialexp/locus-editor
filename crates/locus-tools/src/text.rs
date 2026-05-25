//! Text tool — create and edit text nodes on the canvas.
//!
//! Interaction model:
//!
//! - **Click on empty canvas** to create a new text node and enter editing mode.
//! - **Click on existing text node** to enter editing mode on that node.
//! - **Type** to insert characters at the cursor position.
//! - **Backspace/Delete** to remove characters.
//! - **Left/Right arrows** to move the cursor.
//! - **Home/End** to jump to start/end.
//! - **Enter** or **Escape** to commit the current edit.
//! - Switching tools commits the current edit.

use locus_geom::{Affine, Point};
use locus_scene::{Node, NodeData, NodeId, Scene, TextData};

/// Hit-test radius in screen pixels (divided by zoom for canvas-space radius).
const HIT_RADIUS_SCREEN_PX: f64 = 8.0;

/// Result of a text tool action.
pub enum TextAction {
    /// Nothing happened.
    None,
    /// Editing continues — caller should mark dirty and redraw.
    Continue,
    /// Editing finished. Contains (node_id, original_text_data) for undo.
    Committed(NodeId, TextData),
    /// A new text node was created and editing started.
    Created(NodeId),
    /// Editing cancelled — empty new node was removed.
    Cancelled,
}

/// The text tool's operating mode.
enum TextToolMode {
    /// Not editing any text node.
    Idle,
    /// Actively editing a text node.
    Editing {
        node_id: NodeId,
        /// Cursor position as character index (0 = before first char).
        cursor: usize,
        /// Snapshot of the text data when editing began (for undo).
        original: TextData,
        /// Whether this node was freshly created by this editing session.
        is_new: bool,
    },
}

/// State for the text editing tool.
pub struct TextToolState {
    mode: TextToolMode,
}

impl Default for TextToolState {
    fn default() -> Self {
        Self {
            mode: TextToolMode::Idle,
        }
    }
}

impl TextToolState {
    /// Whether the tool is currently editing a text node.
    pub fn is_editing(&self) -> bool {
        matches!(self.mode, TextToolMode::Editing { .. })
    }

    /// The node currently being edited, if any.
    pub fn editing_node(&self) -> Option<NodeId> {
        match &self.mode {
            TextToolMode::Editing { node_id, .. } => Some(*node_id),
            TextToolMode::Idle => None,
        }
    }

    /// Current cursor position (character index).
    pub fn cursor(&self) -> usize {
        match &self.mode {
            TextToolMode::Editing { cursor, .. } => *cursor,
            TextToolMode::Idle => 0,
        }
    }

    /// Get the cursor X position and vertical extent in the text node's local
    /// coordinate space, for rendering the caret. Returns `(x, top_y, bottom_y)`.
    pub fn cursor_local_position(&self, scene: &Scene) -> Option<(f64, f64, f64)> {
        let TextToolMode::Editing {
            node_id, cursor, ..
        } = &self.mode
        else {
            return None;
        };
        let node = scene.get(*node_id)?;
        let NodeData::Text(text) = &node.data else {
            return None;
        };
        let positions =
            locus_text::cursor_positions(&text.content, &text.font_family, text.font_size);
        let shaped = locus_text::shape_text(&text.content, &text.font_family, text.font_size);
        let x = positions.x_positions.get(*cursor).copied().unwrap_or(0.0);
        // ascent is positive (above baseline), descent is negative (below baseline).
        // In our coordinate system Y is down, so top = -ascent, bottom = -descent.
        let top = -shaped.ascent;
        let bottom = -shaped.descent;
        Some((x, top, bottom))
    }

    /// Get the world transform of the node being edited.
    pub fn editing_world_transform(&self, scene: &Scene) -> Option<Affine> {
        let node_id = self.editing_node()?;
        Some(scene.world_transform(node_id))
    }

    // ── Press ───────────────────────────────────────────────────────

    /// Handle left mouse press at `canvas_pos` (world coordinates).
    pub fn on_press(&mut self, scene: &mut Scene, canvas_pos: [f64; 2], zoom: f64) -> TextAction {
        // If already editing, check what was clicked.
        if let TextToolMode::Editing { node_id, .. } = &self.mode {
            let current_id = *node_id;

            // Check if clicked on the same text node — reposition cursor.
            if self.hit_test_node(scene, current_id, canvas_pos, zoom) {
                self.place_cursor_at(scene, current_id, canvas_pos);
                return TextAction::Continue;
            }

            // Check if clicked on a different text node.
            if let Some(hit_id) = self.hit_test_any_text(scene, canvas_pos, zoom) {
                // Commit current edit first.
                let commit_action = self.commit(scene);

                // Start editing the new node.
                self.start_editing(scene, hit_id, false);
                self.place_cursor_at(scene, hit_id, canvas_pos);

                // Return the commit action (the caller should handle undo for it).
                return commit_action;
            }

            // Clicked on empty space — commit and go idle.
            return self.commit(scene);
        }

        // Idle: check if clicked on an existing text node.
        if let Some(hit_id) = self.hit_test_any_text(scene, canvas_pos, zoom) {
            self.start_editing(scene, hit_id, false);
            self.place_cursor_at(scene, hit_id, canvas_pos);
            return TextAction::Continue;
        }

        // Clicked on empty space — create a new text node.
        let text_data = TextData {
            content: String::new(),
            font_family: "sans-serif".to_string(),
            font_size: 24.0,
            style: locus_scene::Style::default(),
        };
        let node = Node::text("text", text_data);
        let root = scene.root();
        if let Some(node_id) = scene.insert(root, node) {
            // Set the transform to position the text at the click location.
            scene.set_transform(node_id, Affine::translate(canvas_pos[0], canvas_pos[1]));
            self.start_editing(scene, node_id, true);
            return TextAction::Created(node_id);
        }

        TextAction::None
    }

    // ── Key input ──────────────────────────────────────────────────

    /// Handle a character input while editing.
    pub fn on_char(&mut self, scene: &mut Scene, ch: &str) -> TextAction {
        let TextToolMode::Editing {
            node_id, cursor, ..
        } = &mut self.mode
        else {
            return TextAction::None;
        };

        let node_id = *node_id;
        let Some(mut node) = scene.edit(node_id) else {
            return TextAction::None;
        };
        let NodeData::Text(text) = &mut node.data else {
            return TextAction::None;
        };

        // Insert the character(s) at the cursor position.
        let byte_offset = char_to_byte_offset(&text.content, *cursor);
        text.content.insert_str(byte_offset, ch);
        *cursor += ch.chars().count();

        TextAction::Continue
    }

    /// Handle backspace key.
    pub fn on_backspace(&mut self, scene: &mut Scene) -> TextAction {
        let TextToolMode::Editing {
            node_id, cursor, ..
        } = &mut self.mode
        else {
            return TextAction::None;
        };

        if *cursor == 0 {
            return TextAction::None;
        }

        let node_id = *node_id;
        let Some(mut node) = scene.edit(node_id) else {
            return TextAction::None;
        };
        let NodeData::Text(text) = &mut node.data else {
            return TextAction::None;
        };

        let start = char_to_byte_offset(&text.content, *cursor - 1);
        let end = char_to_byte_offset(&text.content, *cursor);
        text.content.replace_range(start..end, "");
        *cursor -= 1;

        TextAction::Continue
    }

    /// Handle delete key.
    pub fn on_delete(&mut self, scene: &mut Scene) -> TextAction {
        let TextToolMode::Editing {
            node_id, cursor, ..
        } = &mut self.mode
        else {
            return TextAction::None;
        };

        let node_id = *node_id;
        let char_count = {
            let Some(node) = scene.get(node_id) else {
                return TextAction::None;
            };
            let NodeData::Text(text) = &node.data else {
                return TextAction::None;
            };
            text.content.chars().count()
        };

        if *cursor >= char_count {
            return TextAction::None;
        }

        let Some(mut node) = scene.edit(node_id) else {
            return TextAction::None;
        };
        let NodeData::Text(text) = &mut node.data else {
            return TextAction::None;
        };

        let start = char_to_byte_offset(&text.content, *cursor);
        let end = char_to_byte_offset(&text.content, *cursor + 1);
        text.content.replace_range(start..end, "");

        TextAction::Continue
    }

    /// Move cursor left.
    pub fn move_left(&mut self) -> TextAction {
        let TextToolMode::Editing { cursor, .. } = &mut self.mode else {
            return TextAction::None;
        };
        if *cursor > 0 {
            *cursor -= 1;
            TextAction::Continue
        } else {
            TextAction::None
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self, scene: &Scene) -> TextAction {
        let TextToolMode::Editing {
            node_id, cursor, ..
        } = &mut self.mode
        else {
            return TextAction::None;
        };
        let Some(node) = scene.get(*node_id) else {
            return TextAction::None;
        };
        let NodeData::Text(text) = &node.data else {
            return TextAction::None;
        };
        let char_count = text.content.chars().count();
        if *cursor < char_count {
            *cursor += 1;
            TextAction::Continue
        } else {
            TextAction::None
        }
    }

    /// Move cursor to start.
    pub fn move_home(&mut self) -> TextAction {
        let TextToolMode::Editing { cursor, .. } = &mut self.mode else {
            return TextAction::None;
        };
        if *cursor != 0 {
            *cursor = 0;
            TextAction::Continue
        } else {
            TextAction::None
        }
    }

    /// Move cursor to end.
    pub fn move_end(&mut self, scene: &Scene) -> TextAction {
        let TextToolMode::Editing {
            node_id, cursor, ..
        } = &mut self.mode
        else {
            return TextAction::None;
        };
        let Some(node) = scene.get(*node_id) else {
            return TextAction::None;
        };
        let NodeData::Text(text) = &node.data else {
            return TextAction::None;
        };
        let char_count = text.content.chars().count();
        if *cursor != char_count {
            *cursor = char_count;
            TextAction::Continue
        } else {
            TextAction::None
        }
    }

    // ── Commit / Cancel ────────────────────────────────────────────

    /// Commit the current edit and return to idle.
    pub fn commit(&mut self, scene: &mut Scene) -> TextAction {
        let TextToolMode::Editing {
            node_id,
            original,
            is_new,
            ..
        } = std::mem::replace(&mut self.mode, TextToolMode::Idle)
        else {
            return TextAction::None;
        };

        // If this was a newly created node and the content is empty, remove it.
        let is_empty = scene
            .get(node_id)
            .and_then(|n| match &n.data {
                NodeData::Text(t) => Some(t.content.is_empty()),
                _ => None,
            })
            .unwrap_or(true);

        if is_new && is_empty {
            scene.remove(node_id);
            return TextAction::Cancelled;
        }

        // Check if anything actually changed.
        let changed = scene
            .get(node_id)
            .and_then(|n| match &n.data {
                NodeData::Text(t) => Some(t.content != original.content),
                _ => None,
            })
            .unwrap_or(false);

        if changed || is_new {
            TextAction::Committed(node_id, original)
        } else {
            TextAction::None
        }
    }

    /// Cancel editing — revert to original text if it was an existing node,
    /// or remove the node if it was newly created.
    pub fn cancel(&mut self, scene: &mut Scene) -> TextAction {
        let TextToolMode::Editing {
            node_id,
            original,
            is_new,
            ..
        } = std::mem::replace(&mut self.mode, TextToolMode::Idle)
        else {
            return TextAction::None;
        };

        if is_new {
            scene.remove(node_id);
            return TextAction::Cancelled;
        }

        // Restore the original text.
        scene.with_text_data_mut(node_id, |text| {
            *text = original;
        });

        TextAction::Cancelled
    }

    // ── Internal helpers ───────────────────────────────────────────

    fn start_editing(&mut self, scene: &Scene, node_id: NodeId, is_new: bool) {
        let original = scene
            .get(node_id)
            .and_then(|n| match &n.data {
                NodeData::Text(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_else(|| TextData {
                content: String::new(),
                font_family: "sans-serif".to_string(),
                font_size: 24.0,
                style: locus_scene::Style::default(),
            });

        let cursor = original.content.chars().count();
        self.mode = TextToolMode::Editing {
            node_id,
            cursor,
            original,
            is_new,
        };
    }

    fn place_cursor_at(&mut self, scene: &Scene, node_id: NodeId, canvas_pos: [f64; 2]) {
        let TextToolMode::Editing { cursor, .. } = &mut self.mode else {
            return;
        };

        let Some(node) = scene.get(node_id) else {
            return;
        };
        let NodeData::Text(text) = &node.data else {
            return;
        };

        // Convert canvas position to the text node's local space.
        let world = scene.world_transform(node_id);
        let inv = world.inverse().unwrap_or(Affine::IDENTITY);
        let local = inv.apply(Point::new(canvas_pos[0], canvas_pos[1]));

        let positions =
            locus_text::cursor_positions(&text.content, &text.font_family, text.font_size);
        *cursor = positions.hit_test(local.x);
    }

    fn hit_test_node(
        &self,
        scene: &Scene,
        node_id: NodeId,
        canvas_pos: [f64; 2],
        _zoom: f64,
    ) -> bool {
        let Some(node) = scene.get(node_id) else {
            return false;
        };
        // Locked / hidden nodes are not interactive — text tool can't pick
        // them up to edit. Same gate as the select tool's hit tests.
        if !node.is_interactive() {
            return false;
        }
        let world = scene.world_transform(node_id);
        let bounds = node.data.visual_bounds(world);
        if bounds.is_empty() {
            // Empty text node — use a small area around the transform origin.
            let origin = world.apply(Point::new(0.0, 0.0));
            let dx = canvas_pos[0] - origin.x;
            let dy = canvas_pos[1] - origin.y;
            let r = HIT_RADIUS_SCREEN_PX / _zoom;
            return dx * dx + dy * dy < r * r;
        }
        bounds.contains_point(Point::new(canvas_pos[0], canvas_pos[1]))
    }

    fn hit_test_any_text(&self, scene: &Scene, canvas_pos: [f64; 2], zoom: f64) -> Option<NodeId> {
        let root = scene.root();
        let root_node = scene.get(root)?;
        // Iterate children in reverse (top-most first).
        for &child_id in root_node.children.iter().rev() {
            let child = scene.get(child_id)?;
            if !child.is_interactive() {
                continue;
            }
            if matches!(&child.data, NodeData::Text(_))
                && self.hit_test_node(scene, child_id, canvas_pos, zoom)
            {
                return Some(child_id);
            }
        }
        None
    }
}

/// Convert a character index to a byte offset in a UTF-8 string.
fn char_to_byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}
