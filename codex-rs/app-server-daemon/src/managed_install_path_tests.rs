use pretty_assertions::assert_eq;

#[test]
fn discovers_package_and_legacy_installs() {
    let home = tempfile::TempDir::new().expect("home");
    let current = home.path().join("packages/standalone/current");
    let legacy = current.join(super::managed_codex_file_name());
    assert_eq!(
        super::managed_codex_bin(home.path()),
        home.path()
            .join("packages/app-server-daemon/current/bin")
            .join(super::managed_codex_file_name())
    );
    std::fs::create_dir_all(&current).expect("current directory");
    std::fs::write(&legacy, b"legacy").expect("legacy executable");
    let state = home.path().join("app-server-daemon");
    std::fs::create_dir(&state).unwrap();
    for name in ["settings.json", "daemon.lock", "app-server.pid.lock"] {
        std::fs::write(state.join(name), b"").unwrap();
    }
    // A CLI install and a previous stop/status operation do not establish ownership.
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/app-server-daemon")
    );
    std::fs::write(state.join("app-server.stderr.log"), b"").unwrap();
    assert_eq!(super::managed_codex_bin(home.path()), legacy);
    let packaged = current.join("bin").join(super::managed_codex_file_name());
    std::fs::create_dir(current.join("bin")).expect("bin directory");
    std::fs::write(&packaged, b"packaged").expect("packaged executable");
    assert_eq!(super::managed_codex_bin(home.path()), packaged);

    std::fs::remove_dir_all(home.path().join("packages/standalone")).unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/standalone")
    );
    std::fs::remove_file(state.join("app-server.stderr.log")).unwrap();
    std::fs::write(state.join("app-server.pid"), b"running daemon").unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/standalone")
    );

    std::fs::write(state.join("daemon.pid"), b"running dedicated daemon").unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/app-server-daemon")
    );
    std::fs::remove_file(state.join("daemon.pid")).unwrap();
    std::fs::write(state.join("daemon.stderr.log"), b"").unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/app-server-daemon")
    );

    #[cfg(unix)]
    {
        let dedicated = home.path().join("packages/app-server-daemon");
        std::fs::write(&dedicated, b"not a directory").expect("unreadable selection");
        assert_eq!(super::package_root(home.path()), dedicated);
    }
}
