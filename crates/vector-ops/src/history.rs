use vector_scene::Scene;

use crate::command::Command;

/// Undo/redo history. Stores a stack of undo commands and a stack of redo commands.
pub struct History {
    undo_stack: Vec<Command>,
    redo_stack: Vec<Command>,
}

impl History {
    pub fn new() -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    /// Execute a command, record its undo, and clear the redo stack.
    pub fn execute(&mut self, cmd: Command, scene: &mut Scene) {
        if let Some(undo) = cmd.apply(scene) {
            self.undo_stack.push(undo);
            self.redo_stack.clear();
        }
    }

    /// Undo the last command.
    pub fn undo(&mut self, scene: &mut Scene) {
        if let Some(undo_cmd) = self.undo_stack.pop()
            && let Some(redo_cmd) = undo_cmd.apply(scene)
        {
            self.redo_stack.push(redo_cmd);
        }
    }

    /// Redo the last undone command.
    pub fn redo(&mut self, scene: &mut Scene) {
        if let Some(redo_cmd) = self.redo_stack.pop()
            && let Some(undo_cmd) = redo_cmd.apply(scene)
        {
            self.undo_stack.push(undo_cmd);
        }
    }

    /// Record a pre-built undo command without executing anything.
    /// Use this when the mutation has already been applied to the scene
    /// directly (e.g. by a continuous drag operation or a tool that manages
    /// its own scene mutations).
    pub fn record_undo(&mut self, undo_cmd: Command) {
        self.undo_stack.push(undo_cmd);
        self.redo_stack.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}
