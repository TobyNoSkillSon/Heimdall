//! Segmentation appends transcript lines; only accepted sends consume a thought.
use anyhow::{ensure, Result};
#[derive(Default, Debug)]
pub struct PendingThought {
    lines: Vec<String>,
    bytes: usize,
}
impl PendingThought {
    pub fn append(&mut self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        let extra = line.len() + usize::from(!self.lines.is_empty());
        ensure!(
            self.bytes + extra <= 65536,
            "pending thought exceeds 64 KiB; retain archive and refuse send until reconciled"
        );
        self.lines.push(line.into());
        self.bytes += extra;
        Ok(())
    }
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    /// Call only after the sender acknowledges consumption. A segment cut, model
    /// failure, source discontinuity, or low completion probability never clears it.
    pub fn sent(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn premature_segments_retain_complete_pending_thought() {
        let mut p = PendingThought::default();
        p.append("I was thinking").unwrap();
        p.append("we could move this tomorrow.").unwrap();
        assert_eq!(p.text(), "I was thinking\nwe could move this tomorrow.");
        assert!(p.append(&"x".repeat(65536)).is_err());
        assert!(p.text().ends_with("tomorrow."));
        p.sent();
        assert!(p.is_empty());
        p.append("A separate thought.").unwrap();
        assert_eq!(p.text(), "A separate thought.");
    }
}
