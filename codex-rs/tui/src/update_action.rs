/// Update action emitted by the TUI after it restores the terminal.
///
/// KAG does not self-update. The only remaining update action is the internal
/// app-server daemon handoff, which reuses the currently running CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    Daemon(DaemonUpdateSource),
}

/// Package source explicitly selected by the user in the daemon menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonUpdateSource {
    PublicStable,
    ThisCli,
}

impl DaemonUpdateSource {
    pub fn command_args(self) -> &'static [&'static str] {
        match self {
            Self::PublicStable => &["app-server", "daemon", "update"],
            Self::ThisCli => &["app-server", "daemon", "update", "--from-cli", "--yes"],
        }
    }
}
