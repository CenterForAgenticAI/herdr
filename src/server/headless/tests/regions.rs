//! Region delivery to client shells: opt-in, viewport-driven sizing, and input gating.

use super::*;

use crate::protocol::region_surface;

fn connect_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    regions: bool,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            regions,
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    (control, render)
}

fn drain(receiver: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<ServerMessage> {
    let mut messages = Vec::new();
    while let Ok(bytes) = receiver.try_recv() {
        let len = bytes.len() as u64;
        let mut cursor = std::io::Cursor::new(bytes);
        while cursor.position() < len {
            messages.push(
                protocol::read_message(&mut cursor, MAX_FRAME_SIZE).expect("decode server message"),
            );
        }
    }
    messages
}

fn region_controls(messages: &[ServerMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter_map(|message| match message {
            ServerMessage::EndpointControl { kind, data }
                if kind.starts_with("endpoint.region") =>
            {
                Some((kind.clone(), data.clone()))
            }
            _ => None,
        })
        .collect()
}

fn region_surfaces(messages: &[ServerMessage]) -> Vec<region_surface::RegionSurface> {
    region_controls(messages)
        .into_iter()
        .filter(|(kind, _)| kind == region_surface::SURFACE_KIND)
        .map(|(_, data)| region_surface::decode_surface(&data).expect("region surface"))
        .collect()
}

fn text_commit(text: &str) -> Vec<crate::protocol::ClientPaneInputEvent> {
    vec![crate::protocol::ClientPaneInputEvent::TextCommit(
        text.into(),
    )]
}

fn region_terminal_id(server: &HeadlessServer, region_id: &crate::region::RegionId) -> String {
    server
        .app
        .state
        .regions
        .get_by_id(region_id.as_str())
        .expect("region")
        .terminal_id
        .to_string()
}

fn region_size(server: &HeadlessServer, region_id: &crate::region::RegionId) -> (u16, u16) {
    let terminal_id = server
        .app
        .state
        .regions
        .get_by_id(region_id.as_str())
        .expect("region")
        .terminal_id
        .clone();
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("region runtime")
        .current_size()
}

fn viewport(server: &mut HeadlessServer, client_id: u64, region_id: &str, cols: u16, rows: u16) {
    server.handle_server_event(ServerEvent::ClientShellRegionViewport {
        client_id,
        viewport: region_surface::RegionViewport {
            region_id: region_id.to_owned(),
            cols,
            rows,
        },
    });
}

#[tokio::test]
async fn shell_without_region_capability_never_receives_region_traffic() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"REGION_LIVE"),
    );
    let (control, render) = connect_shell(&mut server, 7, false);

    server.render_and_stream();
    server.stream_region_surfaces();

    let mut messages = drain(&control);
    messages.extend(drain(&render));
    assert!(region_controls(&messages).is_empty());
    assert!(!server.has_region_viewer());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn region_capable_shell_receives_projection_and_frame() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let region_id = server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"REGION_LIVE"),
    );
    let (control, render) = connect_shell(&mut server, 7, true);

    server.render_and_stream();
    let _ = drain(&render);
    server.stream_region_surfaces();

    let projections = region_controls(&drain(&control))
        .into_iter()
        .filter(|(kind, _)| kind == region_surface::PROJECTION_KIND)
        .map(|(_, data)| region_surface::decode_projection(&data).expect("projection"))
        .collect::<Vec<_>>();
    let projection = projections.last().expect("region projection");
    assert_eq!(projection.regions.len(), 1);
    assert_eq!(projection.regions[0].region_id, region_id.as_str());
    assert_eq!(projection.regions[0].edge, "right");
    assert!(projection.regions[0].visible);

    let surfaces = region_surfaces(&drain(&render));
    let surface = surfaces.last().expect("region frame");
    assert_eq!(surface.region_id, region_id.as_str());
    assert!(frame_text(&surface.frame).contains("REGION_LIVE"));

    // An unchanged region is not resent.
    server.stream_region_surfaces();
    assert!(region_controls(&drain(&control)).is_empty());
    assert!(region_surfaces(&drain(&render)).is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn hidden_region_sends_no_frames() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let region_id = server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"REGION_LIVE"),
    );
    server
        .app
        .state
        .regions
        .get_mut_by_id(region_id.as_str())
        .unwrap()
        .visible = false;
    let (_control, render) = connect_shell(&mut server, 7, true);

    server.render_and_stream();
    let _ = drain(&render);
    server.stream_region_surfaces();
    assert!(region_surfaces(&drain(&render)).is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn reported_viewport_resizes_the_region_terminal() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let region_id = server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"R"),
    );
    let (_control, _render) = connect_shell(&mut server, 7, true);

    viewport(&mut server, 7, region_id.as_str(), 42, 17);
    assert_eq!(region_size(&server, &region_id), (17, 42));

    // A zero viewport means the client hides the region; it never drives the size.
    viewport(&mut server, 7, region_id.as_str(), 0, 0);
    assert_eq!(region_size(&server, &region_id), (17, 42));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn foreground_client_viewport_wins_over_a_later_background_report() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let region_id = server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"R"),
    );
    let (_c1, _r1) = connect_shell(&mut server, 7, true);
    let (_c2, _r2) = connect_shell(&mut server, 8, true);
    server.promote_client_to_foreground(7);

    viewport(&mut server, 7, region_id.as_str(), 40, 20);
    viewport(&mut server, 8, region_id.as_str(), 25, 12);
    assert_eq!(region_size(&server, &region_id), (20, 40));

    server.promote_client_to_foreground(8);
    viewport(&mut server, 7, region_id.as_str(), 41, 20);
    assert_eq!(region_size(&server, &region_id), (12, 25));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn foreground_client_that_hides_the_region_yields_to_a_client_showing_it() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let region_id = server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"R"),
    );
    let (_c1, _r1) = connect_shell(&mut server, 7, true);
    let (_c2, _r2) = connect_shell(&mut server, 8, true);
    server.promote_client_to_foreground(7);

    viewport(&mut server, 7, region_id.as_str(), 0, 0);
    viewport(&mut server, 8, region_id.as_str(), 33, 14);
    assert_eq!(region_size(&server, &region_id), (14, 33));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn viewport_for_an_unknown_region_is_ignored() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let region_id = server.app.install_test_region(
        "plugin.board",
        crate::terminal::TerminalRuntime::test_with_screen_bytes(30, 10, b"R"),
    );
    let (_control, _render) = connect_shell(&mut server, 7, true);

    viewport(&mut server, 7, "region-not-here", 50, 20);
    assert_eq!(region_size(&server, &region_id), (10, 30));
    assert!(server.clients[&7]
        .shell_regions
        .as_ref()
        .unwrap()
        .viewports
        .is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn only_a_region_capable_shell_can_type_into_a_region() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(30, 10);
    let region_id = server.app.install_test_region("plugin.board", runtime);
    let terminal_id = region_terminal_id(&server, &region_id);
    let (_c1, _r1) = connect_shell(&mut server, 7, false);
    let (_c2, _r2) = connect_shell(&mut server, 8, true);

    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: 7,
        terminal_id: terminal_id.clone(),
        events: text_commit("from-legacy"),
    });
    assert!(input.try_recv().is_err());

    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: 8,
        terminal_id,
        events: text_commit("typed"),
    });
    assert_eq!(
        input.try_recv().expect("region input"),
        Bytes::from_static(b"typed")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn hidden_region_rejects_new_input() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(30, 10);
    let region_id = server.app.install_test_region("plugin.board", runtime);
    let terminal_id = region_terminal_id(&server, &region_id);
    server
        .app
        .state
        .regions
        .get_mut_by_id(region_id.as_str())
        .unwrap()
        .visible = false;
    let (_control, _render) = connect_shell(&mut server, 7, true);

    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: 7,
        terminal_id,
        events: text_commit("hidden"),
    });
    assert!(input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}
