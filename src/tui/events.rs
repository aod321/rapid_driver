//! Event handling for TUI.

/// Application events
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEvent {
    /// Continue running
    Continue,
    /// Quit the application
    Quit,
}
