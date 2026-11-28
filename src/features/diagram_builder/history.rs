/// In-memory undo/redo snapshot history for valid Mermaid diagrams.
#[derive(Debug, Default, Clone)]
pub struct DiagramHistory {
    snapshots: Vec<String>,
    cursor: Option<usize>,
}

impl DiagramHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a new valid Mermaid snapshot, discarding any redo branch after the current cursor.
    /// No-ops if `source` is identical to the current active snapshot.
    pub fn push(&mut self, source: String) {
        if self.current() == Some(source.as_str()) {
            return;
        }
        if let Some(idx) = self.cursor {
            self.snapshots.truncate(idx + 1);
        } else {
            self.snapshots.clear();
        }
        self.snapshots.push(source);
        self.cursor = Some(self.snapshots.len() - 1);
    }

    pub fn current(&self) -> Option<&str> {
        self.cursor
            .and_then(|idx| self.snapshots.get(idx))
            .map(String::as_str)
    }

    pub fn can_undo(&self) -> bool {
        matches!(self.cursor, Some(idx) if idx > 0)
    }

    pub fn can_redo(&self) -> bool {
        matches!(self.cursor, Some(idx) if idx + 1 < self.snapshots.len())
    }

    pub fn undo(&mut self) -> Option<&str> {
        let idx = self.cursor?;
        if idx == 0 {
            return None;
        }
        let next = idx - 1;
        self.cursor = Some(next);
        self.snapshots.get(next).map(String::as_str)
    }

    pub fn redo(&mut self) -> Option<&str> {
        let idx = self.cursor?;
        if idx + 1 >= self.snapshots.len() {
            return None;
        }
        let next = idx + 1;
        self.cursor = Some(next);
        self.snapshots.get(next).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    pub fn clear(&mut self) {
        self.snapshots.clear();
        self.cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history_cannot_undo_or_redo() {
        let mut history = DiagramHistory::new();
        assert!(history.is_empty());
        assert_eq!(history.current(), None);
        assert!(!history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.undo(), None);
        assert_eq!(history.redo(), None);
    }

    #[test]
    fn push_undo_redo_and_deduplication_work() {
        let mut history = DiagramHistory::new();
        history.push("flowchart TD\nA-->B".to_string());
        assert!(!history.is_empty());
        assert_eq!(history.current(), Some("flowchart TD\nA-->B"));
        assert!(!history.can_undo());
        assert!(!history.can_redo());

        // Pushing identical snapshot is a no-op
        history.push("flowchart TD\nA-->B".to_string());
        assert!(!history.can_undo());

        history.push("flowchart TD\nA-->B-->C".to_string());
        history.push("flowchart TD\nA-->B-->C-->D".to_string());
        assert!(history.can_undo());
        assert!(!history.can_redo());

        assert_eq!(history.undo(), Some("flowchart TD\nA-->B-->C"));
        assert!(history.can_undo());
        assert!(history.can_redo());

        assert_eq!(history.undo(), Some("flowchart TD\nA-->B"));
        assert!(!history.can_undo());
        assert!(history.can_redo());

        // Repeated undo at start returns None and stays at first snapshot
        assert_eq!(history.undo(), None);
        assert_eq!(history.current(), Some("flowchart TD\nA-->B"));

        assert_eq!(history.redo(), Some("flowchart TD\nA-->B-->C"));
        assert_eq!(history.redo(), Some("flowchart TD\nA-->B-->C-->D"));
        assert!(!history.can_redo());

        // Repeated redo at end returns None
        assert_eq!(history.redo(), None);
        assert_eq!(history.current(), Some("flowchart TD\nA-->B-->C-->D"));
    }

    #[test]
    fn push_after_undo_truncates_redo_branch_and_clear_resets() {
        let mut history = DiagramHistory::new();
        history.push("v1".to_string());
        history.push("v2".to_string());
        history.push("v3".to_string());

        assert_eq!(history.undo(), Some("v2"));
        assert!(history.can_redo());

        history.push("v2b".to_string());
        assert!(!history.can_redo());
        assert_eq!(history.redo(), None);
        assert_eq!(history.current(), Some("v2b"));

        assert_eq!(history.undo(), Some("v2"));
        assert_eq!(history.redo(), Some("v2b"));

        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.current(), None);
        assert!(!history.can_undo());
        assert!(!history.can_redo());
    }
}
