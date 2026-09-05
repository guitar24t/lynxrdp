//! Local transfer controls inside the session window.
use std::path::PathBuf;
use winit::keyboard::{Key, NamedKey};
#[derive(Default)]
pub struct Panel {
    pub open: bool,
    pub remote: String,
    pub local: String,
    pub replace: bool,
    pub field: Option<usize>,
    pub message: String,
    pub pointer_y: f64,
    pub offset: usize,
    painted: Vec<(u64, String)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Download,
    Upload,
    CancelAll,
    Cancel(u64),
    None,
}
impl Panel {
    pub fn key(&mut self, key: &Key) -> Action {
        match key {
            Key::Named(NamedKey::Escape) => {
                self.open = false;
                self.field = None;
            }
            Key::Named(NamedKey::Tab) => {
                self.field = Some(if self.field == Some(0) { 1 } else { 0 })
            }
            Key::Named(NamedKey::F2) => self.field = Some(0),
            Key::Named(NamedKey::F3) => self.field = Some(1),
            Key::Named(NamedKey::F4) => self.replace = !self.replace,
            Key::Named(NamedKey::F5) => return Action::Upload,
            Key::Named(NamedKey::F6) => return Action::CancelAll,
            Key::Named(NamedKey::Enter) => return Action::Download,
            Key::Named(NamedKey::Backspace) => {
                if let Some(text) = self.input() {
                    text.pop();
                }
            }
            Key::Character(text) => {
                if let Some(input) = self.input() {
                    if input.len() + text.len() <= 4096 {
                        input.push_str(text);
                    }
                }
            }
            _ => {}
        }
        Action::None
    }
    pub fn paste(&mut self, text: &str) {
        if let Some(input) = self.input() {
            for ch in text.chars().filter(|c| !c.is_control()) {
                if input.len() + ch.len_utf8() > 4096 {
                    break;
                }
                input.push(ch);
            }
        }
    }
    pub fn scroll(&mut self, delta: f32, total: usize) {
        if delta < 0.0 {
            self.offset = (self.offset + 1).min(total.saturating_sub(1));
        }
        if delta > 0.0 {
            self.offset = self.offset.saturating_sub(1);
        }
    }
    fn input(&mut self) -> Option<&mut String> {
        match self.field {
            Some(0) => Some(&mut self.remote),
            Some(1) => Some(&mut self.local),
            _ => None,
        }
    }
    pub fn destination(&self) -> Result<PathBuf, String> {
        if self.remote.is_empty() || self.local.is_empty() {
            return Err("Enter both the remote file and local destination.".into());
        }
        Ok(PathBuf::from(&self.local))
    }
    /// Rows have the same indexes for painting and pointer hit testing.
    pub fn lines(&mut self, queued: usize, active: &[(u64, String)]) -> Vec<String> {
        self.painted = active.to_vec();
        let mut lines = vec![
            "Transfers                         [Close / Esc]".into(),
            format!(
                "[F2] Remote file: {}{}",
                if self.field == Some(0) { "> " } else { "" },
                self.remote
            ),
            format!(
                "[F3] Local path: {}{}",
                if self.field == Some(1) { "> " } else { "" },
                self.local
            ),
            format!(
                "[F4] Existing files: {}",
                if self.replace {
                    "REPLACE after receipt"
                } else {
                    "KEEP (refuse overwrite)"
                }
            ),
            "[Enter] Download file".into(),
            format!("[F5] Start {queued} queued uploads (drop files to queue)"),
            "[F6] Cancel all transfers".into(),
            self.message.clone(),
        ];
        lines.extend(
            active
                .iter()
                .skip(self.offset.min(active.len().saturating_sub(1)))
                .map(|(_, text)| format!("[Cancel] {text}")),
        );
        lines
    }
    pub fn click(&mut self, row: usize) -> Action {
        let active = &self.painted;
        match row {
            0 => self.open = false,
            1 => self.field = Some(0),
            2 => self.field = Some(1),
            3 => self.replace = !self.replace,
            4 => return Action::Download,
            5 => return Action::Upload,
            6 => return Action::CancelAll,
            n if n >= 8 => {
                return active
                    .get(n - 8 + self.offset.min(active.len().saturating_sub(1)))
                    .map(|(id, _)| Action::Cancel(*id))
                    .unwrap_or(Action::None)
            }
            _ => {}
        }
        Action::None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overwrite_requires_a_choice_and_cancel_targets_the_clicked_transfer() {
        let mut p = Panel::default();
        assert!(!p.replace);
        p.click(3);
        assert!(p.replace);
        p.lines(0, &[(12, "first".into()), (77, "second".into())]);
        assert_eq!(p.click(9), Action::Cancel(77));
        assert!(p.destination().is_err());
    }
}
