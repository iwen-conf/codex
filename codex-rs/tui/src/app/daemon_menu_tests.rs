use super::*;
use crate::app::test_support::make_test_app;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use crate::status::remote_connection::RemoteConnectionStatus;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn daemon_version_notice_preserves_manual_update_guidance() {
    let mut app = make_test_app().await;
    app.app_server_target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: true,
        endpoint: crate::RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::relative_to_current_dir("codex.sock").unwrap(),
        },
    };
    let view = app.agents_overview_view(Vec::new(), /*selected_thread_id*/ None);
    app.chat_widget.show_bottom_pane_view(Box::new(view));
    let mut notices = Vec::new();
    for (client, server, comparison) in [
        ("0.155.0-alpha.23", "0.155.0-alpha.22", "older than"),
        ("0.155.0-alpha.23", "0.156.0", "different from"),
        ("0.0.0", "0.156.0", "different from"),
    ] {
        app.local_settings.tui.show_server_version_notice = true;
        assert_eq!(
            app.initialize_server_version_notice(client, Some(server)),
            Some(format!(
                "A background Codex service is running v{server}, {comparison} your Codex CLI v{client}."
            ))
        );
        let overview = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        notices.push(
            overview
                .lines()
                .find(|line| line.contains("Service v"))
                .unwrap()
                .trim()
                .to_string(),
        );
        assert_eq!(app.pending_update_action, None);

        app.local_settings.tui.show_server_version_notice = false;
        assert_eq!(
            app.initialize_server_version_notice(client, Some(server)),
            None
        );
        assert_eq!(
            app.agents_overview
                .view_state
                .lock()
                .unwrap()
                .server_version_notice,
            None
        );
    }
    insta::assert_snapshot!(notices.join("\n"), @"
    Service v0.155.0-alpha.22 < Codex CLI v0.155.0-alpha.23 · /daemon
    Service v0.156.0 ≠ Codex CLI v0.155.0-alpha.23 · /daemon
    Service v0.156.0 ≠ Codex CLI v0.0.0 · /daemon
    ");
}

#[tokio::test]
async fn daemon_menu_is_read_only_and_confirmation_can_cancel_or_handoff() {
    let mut app = make_test_app().await;
    let (chat, _, mut rx, _) = make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat;
    let package = tempfile::tempdir().unwrap();
    std::fs::create_dir(package.path().join("bin")).unwrap();
    std::fs::write(package.path().join("bin/codex"), "CLI").unwrap();
    std::fs::write(package.path().join("codex-package.json"), "{}").unwrap();
    app.daemon_cli_executable =
        Some(AbsolutePathBuf::from_absolute_path(package.path().join("bin/codex")).unwrap());
    app.app_server_target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: true,
        endpoint: crate::RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::relative_to_current_dir("codex.sock").unwrap(),
        },
    };
    app.chat_widget.remote_connection = Some(RemoteConnectionStatus {
        address: "local".into(),
        version: "v0.153.0".into(),
        is_local_daemon: true,
    });

    app.open_daemon_menu();
    let menu = render_bottom_popup(&app.chat_widget, /*width*/ 100);
    assert!(menu.contains("Use this KAG CLI build"));
    assert!(!menu.contains("public stable"));
    assert!(rx.try_recv().is_err());

    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(matches!(
        rx.try_recv().unwrap(),
        AppEvent::ConfirmDaemonUpdate(DaemonUpdateSource::ThisCli)
    ));

    app.daemon_cli_executable = Some(
        AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
            r"C:\cli-build\bin\codex"
        } else {
            "/x/cli-build/bin/codex"
        })
        .unwrap(),
    );
    app.confirm_daemon_update(DaemonUpdateSource::ThisCli);
    let confirmation = render_bottom_popup(&app.chat_widget, /*width*/ 100)
        .replace(r"C:\cli-build\bin\codex", "/x/cli-build/bin/codex");
    assert!(confirmation.contains("Use this KAG CLI package"));
    assert!(confirmation.contains("No public Codex update source is used"));

    // The default choice cancels without emitting an update or exiting.
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err());
    assert_eq!(app.pending_update_action, None);

    app.confirm_daemon_update(DaemonUpdateSource::ThisCli);
    app.chat_widget.handle_key_event(KeyCode::Down.into());
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(matches!(
        rx.try_recv().unwrap(),
        AppEvent::RunDaemonUpdate(DaemonUpdateSource::ThisCli)
    ));

    // Maintenance remains available when startup selects the embedded server.
    app.app_server_target = AppServerTarget::Embedded;
    app.chat_widget.remote_connection = None;
    app.open_daemon_menu();
    let disconnected = render_bottom_popup(&app.chat_widget, /*width*/ 80);
    assert!(disconnected.contains("Use this KAG CLI build"));
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(matches!(
        rx.try_recv().unwrap(),
        AppEvent::ConfirmDaemonUpdate(DaemonUpdateSource::ThisCli)
    ));

    std::fs::remove_file(package.path().join("codex-package.json")).unwrap();
    app.daemon_cli_executable =
        Some(AbsolutePathBuf::from_absolute_path(package.path().join("bin/codex-tui")).unwrap());
    app.open_daemon_menu();
    let unpackaged = render_bottom_popup(&app.chat_widget, /*width*/ 80);
    assert!(unpackaged.contains("This CLI has no local package to copy"));
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err());

    app.daemon_cli_executable = None;
    app.open_daemon_menu();
    let no_cli = render_bottom_popup(&app.chat_widget, /*width*/ 80);
    assert!(no_cli.contains("Run the Codex CLI to manage the daemon from this menu."));
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn unavailable_daemon_menu_offers_guidance_without_update_actions() {
    let mut app = make_test_app().await;
    app.daemon_cli_executable =
        Some(AbsolutePathBuf::from_absolute_path(std::env::current_exe().unwrap()).unwrap());
    let (chat, _, mut rx, _) = make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat;
    app.app_server_target = AppServerTarget::Remote {
        endpoint: crate::RemoteAppServerEndpoint::WebSocket {
            websocket_url: "ws://example.test:1234".into(),
            auth_token: None,
        },
    };
    app.chat_widget.remote_connection = Some(RemoteConnectionStatus {
        address: "ws://example.test:1234".into(),
        version: "v0.153.0".into(),
        is_local_daemon: false,
    });
    app.open_daemon_menu();
    insta::assert_snapshot!(
        "daemon_remote_guidance",
        render_bottom_popup(&app.chat_widget, /*width*/ 80)
    );
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err());
    assert_eq!(app.pending_update_action, None);
}
