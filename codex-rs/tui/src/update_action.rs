/// Update action emitted by the TUI after it restores the terminal.
///
/// KAG does not self-update. The only remaining update action is the internal
/// app-server daemon handoff, which reuses the currently running CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    Daemon(DaemonUpdateSource),
}

/// Local daemon maintenance source.
///
/// KAG never downloads or restores an official Codex release. Daemon refreshes
/// always copy the package belonging to the currently running KAG CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonUpdateSource {
    ThisCli,
}

impl DaemonUpdateSource {
    pub fn command_args(self) -> &'static [&'static str] {
        match self {
            Self::ThisCli => &["app-server", "daemon", "update", "--yes"],
        }
    }
}
