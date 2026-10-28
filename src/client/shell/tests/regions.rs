//! Client side of desktop regions: layout, routing, mouse and the capability gate.

use super::*;
use crate::protocol::region_surface::{ClientRegion, RegionProjection, RegionSurface};

const REGION_ID: &str = "region_1";
const REGION_TERMINAL: &str = "terminal-region";

fn region(size: u16, visible: bool, focused: bool) -> ClientRegion {
    ClientRegion {
        region_id: REGION_ID.into(),
        terminal_id: REGION_TERMINAL.into(),
        plugin_id: "notes".into(),
        entrypoint: "notes".into(),
        title: "notes".into(),
        edge: "right".into(),
        scope: "session".into(),
        size,
        min_size: 8,
        max_size: 200,
        visible,
        focused,
        mouse_reporting: false,
    }
}

fn projection(revision: u64, regions: Vec<ClientRegion>) -> RegionProjection {
    let focused = regions
        .iter()
        .find(|region| region.focused)
        .map(|region| region.region_id.clone());
    RegionProjection {
        boot_id: "boot-1".into(),
        revision,
        regions,
        focused,
    }
}

fn region_surface() -> RegionSurface {
    RegionSurface {
        region_id: REGION_ID.into(),
        terminal_id: REGION_TERMINAL.into(),
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(
            &Buffer::with_lines(["NOTE"]),
            None,
            &[],
        ),
    }
}

fn shell() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn shell_with_region(size: u16, visible: bool, focused: bool) -> ClientShellState {
    let mut state = shell();
    state.set_endpoint_regions_supported(&ClientEndpointId::Local, true);
    assert!(state.set_endpoint_region_projection(
        &ClientEndpointId::Local,
        projection(1, vec![region(size, visible, focused)]),
    ));
    state
}

fn endpoint_methods(input: &ClientShellInput) -> Vec<crate::api::schema::Method> {
    input
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(request.method.clone()),
            _ => None,
        })
        .collect()
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

#[test]
fn region_takes_its_width_from_the_right_of_the_tab_area() {
    let baseline = shell().layout(106, 20);
    let state = shell_with_region(30, true, false);
    let layout = state.layout(106, 20);

    assert_eq!(layout.region.width, 30);
    assert_eq!(layout.region.height, 20, "region runs the full height");
    assert_eq!(
        layout.region.x + layout.region.width,
        baseline.pane_surface.x + baseline.pane_surface.width,
        "region sits at the right edge"
    );
    assert_eq!(layout.pane_surface.width, baseline.pane_surface.width - 30);
    assert_eq!(
        layout.tab_bar.width,
        baseline.tab_bar.width - 30,
        "the tab bar does not run above the region"
    );
    assert_eq!(layout.sidebar, baseline.sidebar);
}

#[test]
fn client_without_the_capability_lays_out_exactly_as_before() {
    let baseline = shell().layout(106, 20);
    let mut state = shell();
    // The endpoint never advertised `region_surface`: a projection must not change anything.
    assert!(!state.set_endpoint_region_projection(
        &ClientEndpointId::Local,
        projection(1, vec![region(30, true, true)]),
    ));
    assert!(!state.set_endpoint_region_surface(&ClientEndpointId::Local, region_surface()));

    assert_eq!(state.layout(106, 20), baseline);
    state.compose(106, 20).expect("frame");
    assert!(state.hits.region.is_none());
    assert!(state.take_region_viewport_report().is_none());
    let input = state.handle_input_bytes(b"x");
    assert!(matches!(
        &input.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "pane_1"
    ));
}

#[test]
fn hidden_region_and_mobile_layout_take_no_space() {
    let baseline = shell().layout(106, 20);
    let hidden = shell_with_region(30, false, false);
    assert_eq!(hidden.layout(106, 20), baseline);

    let visible = shell_with_region(30, true, false);
    let threshold = ClientShellConfig::from_config(&Config::default()).mobile_width_threshold;
    assert_eq!(
        visible.layout(threshold, 20),
        shell().layout(threshold, 20),
        "mobile layout has no region"
    );
    assert!(visible.layout(threshold, 20).region.is_empty());
}

#[test]
fn hostile_region_size_never_consumes_the_tab_area() {
    let baseline = shell().layout(106, 20);
    let state = shell_with_region(u16::MAX, true, true);
    assert_eq!(state.layout(106, 20), baseline);
    // Hidden for this frame, so focus falls back to the pane.
    let mut state = state;
    state.compose(106, 20).expect("frame");
    let input = state.handle_input_bytes(b"x");
    assert!(matches!(
        &input.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "pane_1"
    ));
}

#[test]
fn compose_draws_the_region_terminal_inside_its_border() {
    let mut state = shell_with_region(30, true, false);
    assert!(state.set_endpoint_region_surface(&ClientEndpointId::Local, region_surface()));
    let frame = state.compose(106, 20).expect("frame");
    let hit = state.hits.region.clone().expect("region hit");
    let layout = state.layout(106, 20);
    assert_eq!(hit.outer, layout.region);
    let inner = hit.terminal.inner_rect;
    assert_eq!(
        (inner.x, inner.y),
        (layout.region.x + 1, layout.region.y + 1)
    );
    let start = usize::from(inner.y) * usize::from(frame.width) + usize::from(inner.x);
    let drawn = frame.cells[start..start + 4]
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert_eq!(drawn, "NOTE");
    let rows = frame_rows(&frame);
    assert!(
        rows[usize::from(layout.region.y)].contains("notes"),
        "title on the border"
    );
    assert!(
        rows.iter().any(|row| row.contains("LIVE")),
        "panes still drawn"
    );
}

#[test]
fn focused_region_receives_keys_text_and_paste() {
    let mut state = shell_with_region(30, true, true);
    state.compose(106, 20).expect("frame");

    let keys = state.handle_input_bytes(b"x");
    assert!(matches!(
        &keys.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == REGION_TERMINAL
    ));
    let text = state.handle_raw_events(vec![RawInputEvent::Text(crate::input::TextCommit::new(
        "ime",
    ))]);
    assert!(matches!(
        &text.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, events }]
            if terminal_id == REGION_TERMINAL
                && matches!(&events[..], [ClientPaneInputEvent::TextCommit(value)] if value == "ime")
    ));
    let paste = state.handle_raw_events(vec![RawInputEvent::Paste("clip".into())]);
    assert!(matches!(
        &paste.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, events }]
            if terminal_id == REGION_TERMINAL
                && matches!(&events[..], [ClientPaneInputEvent::Paste(value)] if value == "clip")
    ));
}

#[test]
fn unfocused_region_leaves_keys_with_the_pane() {
    let mut state = shell_with_region(30, true, false);
    state.compose(106, 20).expect("frame");
    let keys = state.handle_input_bytes(b"x");
    assert!(matches!(
        &keys.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "pane_1"
    ));
}

#[test]
fn prefix_key_still_reaches_the_shell_while_a_region_is_focused() {
    let mut state = shell_with_region(30, true, true);
    state.compose(106, 20).expect("frame");

    let prefix = state.handle_input_bytes(b"\x02");
    assert!(
        prefix.requests.is_empty(),
        "prefix is not sent to the region"
    );
    assert_eq!(state.mode, ClientShellMode::Prefix);

    // Prefix twice sends the literal prefix key to the focused terminal: the region.
    let again = state.handle_input_bytes(b"\x02");
    assert!(matches!(
        &again.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == REGION_TERMINAL
    ));
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn click_inside_an_unfocused_region_requests_region_focus() {
    let mut state = shell_with_region(30, true, false);
    state.compose(106, 20).expect("frame");
    let hit = state.hits.region.clone().expect("region hit");

    let click = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        hit.terminal.inner_rect.x + 2,
        hit.terminal.inner_rect.y + 1,
    )]);
    let methods = endpoint_methods(&click);
    assert!(
        matches!(
            &methods[..],
            [crate::api::schema::Method::RegionFocus(target)] if target.region_id == REGION_ID
        ),
        "{methods:?}"
    );
    assert!(
        click.requests.is_empty(),
        "no mouse report without mouse reporting"
    );
}

#[test]
fn click_on_a_pane_does_not_touch_the_region() {
    let mut state = shell_with_region(30, true, true);
    state.compose(106, 20).expect("frame");
    let pane = state.hits.panes[0].clone();
    let click = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        pane.inner_rect.x,
        pane.inner_rect.y,
    )]);
    let methods = endpoint_methods(&click);
    assert!(
        methods
            .iter()
            .all(|method| !matches!(method, crate::api::schema::Method::RegionFocus(_))),
        "{methods:?}"
    );
}

#[test]
fn wheel_and_mouse_reports_go_to_a_reporting_region_terminal() {
    let mut state = shell();
    state.set_endpoint_regions_supported(&ClientEndpointId::Local, true);
    let mut reporting = region(30, true, true);
    reporting.mouse_reporting = true;
    state.set_endpoint_region_projection(&ClientEndpointId::Local, projection(1, vec![reporting]));
    state.compose(106, 20).expect("frame");
    let inner = state
        .hits
        .region
        .clone()
        .expect("region hit")
        .terminal
        .inner_rect;

    let wheel = state.handle_raw_events(vec![mouse(
        MouseEventKind::ScrollDown,
        inner.x + 1,
        inner.y + 1,
    )]);
    assert!(matches!(
        &wheel.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == REGION_TERMINAL
    ));
    let click = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        inner.x + 3,
        inner.y + 1,
    )]);
    assert!(matches!(
        &click.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, events }]
            if terminal_id == REGION_TERMINAL
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        position: ClientMousePosition::Cell { column: 3, row: 1 },
                        ..
                    }]
                )
    ));
    let release = state.handle_raw_events(vec![mouse(
        MouseEventKind::Up(MouseButton::Left),
        inner.x + 3,
        inner.y + 1,
    )]);
    assert!(matches!(
        &release.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == REGION_TERMINAL
    ));
}

#[test]
fn dragging_the_left_border_requests_a_resize() {
    let mut state = shell_with_region(30, true, false);
    state.compose(106, 20).expect("frame");
    let outer = state.hits.region.clone().expect("region hit").outer;
    let right_edge = outer.x + outer.width;

    let press = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        outer.x,
        5,
    )]);
    assert!(
        endpoint_methods(&press).is_empty(),
        "pressing the border does not focus"
    );
    let drag = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        outer.x - 6,
        5,
    )]);
    let methods = endpoint_methods(&drag);
    assert!(
        matches!(
            &methods[..],
            [crate::api::schema::Method::RegionResize(params)]
                if params.region_id == REGION_ID && params.size == 36
        ),
        "{methods:?}"
    );
    assert_eq!(u32::from(right_edge - (outer.x - 6)), 36);

    // The same column twice sends nothing new; release ends the drag.
    let same = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        outer.x - 6,
        5,
    )]);
    assert!(endpoint_methods(&same).is_empty());
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Up(MouseButton::Left),
        outer.x - 6,
        5,
    )]);
    assert!(state.chrome_drag.is_none());
}

#[test]
fn toggle_and_focus_region_bindings_send_region_methods() {
    let mut state = shell_with_region(30, true, false);
    state.compose(106, 20).expect("frame");
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusRegion),
        &mut outcome,
    );
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ToggleRegion),
        &mut outcome,
    );
    let methods = endpoint_methods(&outcome);
    assert!(
        matches!(
            &methods[..],
            [
                crate::api::schema::Method::RegionFocus(focus),
                crate::api::schema::Method::RegionHide(hide),
            ] if focus.region_id == REGION_ID && hide.region_id == REGION_ID
        ),
        "{methods:?}"
    );

    // Focused: focus_region returns to the last pane. Hidden: toggle shows it.
    let mut focused = shell_with_region(30, true, true);
    let mut outcome = ClientShellInput::default();
    focused.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusRegion),
        &mut outcome,
    );
    let mut hidden = shell_with_region(30, false, false);
    hidden.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ToggleRegion),
        &mut outcome,
    );
    let methods = endpoint_methods(&outcome);
    assert!(
        matches!(
            &methods[..],
            [
                crate::api::schema::Method::PaneFocus(pane),
                crate::api::schema::Method::RegionShow(show),
            ] if pane.pane_id == "pane_1" && show.region_id == REGION_ID
        ),
        "{methods:?}"
    );
}

#[test]
fn region_bindings_do_nothing_without_a_region() {
    let mut state = shell();
    state.compose(106, 20).expect("frame");
    let mut outcome = ClientShellInput::default();
    for action in [
        crate::input::KeybindAction::FocusRegion,
        crate::input::KeybindAction::ToggleRegion,
    ] {
        state.record_binding(crate::input::KeybindMatch::Action(action), &mut outcome);
    }
    assert!(outcome.actions.is_empty());
}

#[test]
fn viewport_is_reported_once_per_size_and_zero_when_hidden_for_the_frame() {
    let mut state = shell_with_region(30, true, false);
    state.compose(106, 20).expect("frame");
    let (endpoint, message) = state.take_region_viewport_report().expect("viewport");
    assert_eq!(endpoint, ClientEndpointId::Local);
    let ClientMessage::EndpointControl { kind, data } = message else {
        panic!("viewport rides EndpointControl");
    };
    assert_eq!(kind, crate::protocol::region_surface::VIEWPORT_KIND);
    let viewport = crate::protocol::region_surface::decode_viewport(&data).expect("viewport");
    assert_eq!(
        (viewport.cols, viewport.rows),
        (28, 18),
        "inner size of a 30x20 region"
    );

    state.compose(106, 20).expect("frame");
    assert!(
        state.take_region_viewport_report().is_none(),
        "unchanged size is not resent"
    );

    // Too narrow for the region: it is hidden for this frame and reports zero.
    state.compose(40, 20).expect("frame");
    let (_, message) = state.take_region_viewport_report().expect("zero viewport");
    let ClientMessage::EndpointControl { data, .. } = message else {
        panic!("viewport rides EndpointControl");
    };
    let viewport = crate::protocol::region_surface::decode_viewport(&data).expect("viewport");
    assert_eq!((viewport.cols, viewport.rows), (0, 0));
}

#[test]
fn older_same_boot_projection_is_ignored() {
    let mut state = shell_with_region(30, true, false);
    assert!(!state.set_endpoint_region_projection(
        &ClientEndpointId::Local,
        projection(1, vec![region(50, true, true)]),
    ));
    assert_eq!(state.layout(106, 20).region.width, 30);
    assert!(
        state.set_endpoint_region_projection(&ClientEndpointId::Local, projection(2, Vec::new()),)
    );
    assert!(state.layout(106, 20).region.is_empty());
}

#[test]
fn disconnect_forgets_the_endpoint_regions() {
    let mut state = shell_with_region(30, true, true);
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert!(state.layout(106, 20).region.is_empty());
    assert!(
        !state.set_endpoint_region_projection(
            &ClientEndpointId::Local,
            projection(5, vec![region(30, true, true)]),
        ),
        "a reconnect must re-advertise the capability first"
    );
}
