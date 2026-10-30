//! Client side of desktop regions: layout, routing, mouse and the capability gate.

use super::*;
use crate::protocol::region_surface::{ClientRegion, RegionProjection, RegionSurface};

const REGION_ID: &str = "region_1";
const REGION_TERMINAL: &str = "terminal-region";
const BOTTOM_REGION_ID: &str = "region_2";
const BOTTOM_REGION_TERMINAL: &str = "terminal-bottom-region";

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

fn bottom_region(size: u16, visible: bool, focused: bool) -> ClientRegion {
    ClientRegion {
        region_id: BOTTOM_REGION_ID.into(),
        terminal_id: BOTTOM_REGION_TERMINAL.into(),
        title: "log".into(),
        edge: "bottom".into(),
        ..region(size, visible, focused)
    }
}

fn shell_with_regions(regions: Vec<ClientRegion>) -> ClientShellState {
    let mut state = shell();
    state.set_endpoint_regions_supported(&ClientEndpointId::Local, true);
    assert!(state.set_endpoint_region_projection(&ClientEndpointId::Local, projection(1, regions),));
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

    assert_eq!(layout.right_region.width, 30);
    assert_eq!(
        layout.right_region.height, 20,
        "region runs the full height"
    );
    assert_eq!(
        layout.right_region.x + layout.right_region.width,
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
    assert!(state.hits.regions.is_empty());
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
    assert!(visible.layout(threshold, 20).right_region.is_empty());
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
    let hit = state.hits.regions.first().cloned().expect("region hit");
    let layout = state.layout(106, 20);
    assert_eq!(hit.outer, layout.right_region);
    let inner = hit.terminal.inner_rect;
    assert_eq!(
        (inner.x, inner.y),
        (layout.right_region.x + 1, layout.right_region.y + 1)
    );
    let start = usize::from(inner.y) * usize::from(frame.width) + usize::from(inner.x);
    let drawn = frame.cells[start..start + 4]
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert_eq!(drawn, "NOTE");
    let rows = frame_rows(&frame);
    assert!(
        rows[usize::from(layout.right_region.y)].contains("notes"),
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
    let hit = state.hits.regions.first().cloned().expect("region hit");

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
        .regions
        .first()
        .cloned()
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
    let outer = state
        .hits
        .regions
        .first()
        .cloned()
        .expect("region hit")
        .outer;
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
    assert_eq!(state.layout(106, 20).right_region.width, 30);
    assert!(
        state.set_endpoint_region_projection(&ClientEndpointId::Local, projection(2, Vec::new()),)
    );
    assert!(state.layout(106, 20).right_region.is_empty());
}

#[test]
fn disconnect_forgets_the_endpoint_regions() {
    let mut state = shell_with_region(30, true, true);
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert!(state.layout(106, 20).right_region.is_empty());
    assert!(
        !state.set_endpoint_region_projection(
            &ClientEndpointId::Local,
            projection(5, vec![region(30, true, true)]),
        ),
        "a reconnect must re-advertise the capability first"
    );
}

#[test]
fn bottom_region_takes_its_height_from_the_bottom_of_the_tab_area() {
    let baseline = shell().layout(106, 30);
    let state = shell_with_regions(vec![bottom_region(8, true, false)]);
    let layout = state.layout(106, 30);

    assert_eq!(layout.bottom_region.height, 8);
    assert_eq!(layout.bottom_region.x, baseline.pane_surface.x);
    assert_eq!(layout.bottom_region.width, baseline.pane_surface.width);
    assert_eq!(
        layout.bottom_region.y + layout.bottom_region.height,
        30,
        "region sits at the bottom edge"
    );
    assert_eq!(layout.pane_surface.height, baseline.pane_surface.height - 8);
    assert!(
        layout.pane_surface.y + layout.pane_surface.height <= layout.bottom_region.y,
        "pane surface stays above the region"
    );
    assert!(
        layout.tab_bar.y + layout.tab_bar.height <= layout.bottom_region.y,
        "tab bar stays above the region"
    );
    assert!(layout.right_region.is_empty());
}

#[test]
fn bottom_region_sits_beside_a_right_region() {
    let state = shell_with_regions(vec![region(30, true, false), bottom_region(8, true, false)]);
    let layout = state.layout(106, 30);

    assert_eq!(
        layout.right_region.height, 30,
        "right region keeps full height"
    );
    assert_eq!(
        layout.bottom_region.x + layout.bottom_region.width,
        layout.right_region.x,
        "bottom region ends where the right region starts"
    );
    assert_eq!(layout.bottom_region.width, layout.pane_surface.width);
}

#[test]
fn bottom_region_with_a_bottom_tab_bar_keeps_the_bar_above_the_region() {
    let mut config = Config::default();
    config.ui.tab_bar_position = crate::config::TabBarPositionConfig::Bottom;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_regions_supported(&ClientEndpointId::Local, true);
    state.set_endpoint_region_projection(
        &ClientEndpointId::Local,
        projection(1, vec![bottom_region(8, true, false)]),
    );
    let layout = state.layout(106, 30);
    assert_eq!(layout.tab_bar.height, 1, "fixture shows the tab bar");
    assert_eq!(
        layout.tab_bar.y + layout.tab_bar.height,
        layout.bottom_region.y,
        "tab bar sits directly above the region"
    );
    assert_eq!(layout.pane_surface.y, 0);
    assert!(layout.pane_surface.y + layout.pane_surface.height <= layout.bottom_region.y);
}

#[test]
fn both_regions_are_drawn_hit_and_reported() {
    let mut state =
        shell_with_regions(vec![region(30, true, false), bottom_region(8, true, false)]);
    state.compose(106, 30).expect("frame");
    let layout = state.layout(106, 30);
    let outers: Vec<_> = state.hits.regions.iter().map(|hit| hit.outer).collect();
    assert_eq!(outers, vec![layout.right_region, layout.bottom_region]);

    let mut reported = Vec::new();
    while let Some((_, message)) = state.take_region_viewport_report() {
        let ClientMessage::EndpointControl { data, .. } = message else {
            panic!("viewport rides EndpointControl");
        };
        let viewport = crate::protocol::region_surface::decode_viewport(&data).expect("viewport");
        reported.push((viewport.region_id, viewport.cols, viewport.rows));
    }
    reported.sort();
    let bottom_inner = crate::region::region_inner(layout.bottom_region);
    let right_inner = crate::region::region_inner(layout.right_region);
    assert_eq!(
        reported,
        vec![
            (REGION_ID.to_owned(), right_inner.width, right_inner.height),
            (
                BOTTOM_REGION_ID.to_owned(),
                bottom_inner.width,
                bottom_inner.height
            ),
        ]
    );
}

#[test]
fn dragging_the_top_border_of_a_bottom_region_requests_a_resize() {
    let mut state = shell_with_regions(vec![bottom_region(8, true, false)]);
    state.compose(106, 30).expect("frame");
    let outer = state
        .hits
        .regions
        .first()
        .cloned()
        .expect("region hit")
        .outer;
    let bottom_edge = outer.y + outer.height;
    let column = outer.x + 5;

    // A press on the left column of a bottom region is not a border.
    let side = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        outer.x,
        outer.y + 2,
    )]);
    assert!(state.chrome_drag.is_none());
    drop(side);

    let press = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        column,
        outer.y,
    )]);
    assert!(
        endpoint_methods(&press).is_empty(),
        "pressing the border does not focus"
    );
    let drag = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        column,
        outer.y - 4,
    )]);
    let methods = endpoint_methods(&drag);
    assert!(
        matches!(
            &methods[..],
            [crate::api::schema::Method::RegionResize(params)]
                if params.region_id == BOTTOM_REGION_ID && params.size == 12
        ),
        "{methods:?}"
    );
    assert_eq!(bottom_edge - (outer.y - 4), 12);
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Up(MouseButton::Left),
        column,
        outer.y - 4,
    )]);
    assert!(state.chrome_drag.is_none());
}

fn drag_border_to(state: &mut ClientShellState, column: u16, row: u16) -> u32 {
    let drag = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        column,
        row,
    )]);
    let methods = endpoint_methods(&drag);
    let [crate::api::schema::Method::RegionResize(params)] = &methods[..] else {
        panic!("expected one resize: {methods:?}");
    };
    params.size
}

fn shown_after_resize(state: &mut ClientShellState, revision: u64, region: ClientRegion) -> bool {
    let edge = if region.edge == "bottom" {
        crate::region::RegionEdge::Bottom
    } else {
        crate::region::RegionEdge::Right
    };
    state.set_endpoint_region_projection(
        &ClientEndpointId::Local,
        projection(revision, vec![region]),
    );
    !state.layout(106, 30).region(edge).is_empty()
}

// A drag past the tab area's minimum stops there instead of asking for a size the layout
// would hide, which made the region vanish under the pointer in a narrow window.
#[test]
fn dragging_a_right_border_stops_at_the_tab_area_minimum() {
    let mut state = shell_with_region(30, true, false);
    state.compose(106, 30).expect("frame");
    let outer = state
        .hits
        .regions
        .first()
        .cloned()
        .expect("region hit")
        .outer;
    let far_edge = outer.x + outer.width;
    let tab_x = state.layout(106, 30).pane_surface.x;
    let widest = far_edge - tab_x - crate::region::MIN_TAB_SURFACE_WIDTH;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        outer.x,
        5,
    )]);

    assert_eq!(drag_border_to(&mut state, tab_x + 2, 5), u32::from(widest));
    assert!(shown_after_resize(
        &mut state,
        2,
        region(widest, true, false)
    ));
    assert!(
        !shown_after_resize(&mut state, 3, region(widest + 1, true, false)),
        "one cell wider hides the region, so the cap sits exactly at the limit"
    );
}

#[test]
fn dragging_a_bottom_border_stops_at_the_tab_area_minimum() {
    let mut state = shell_with_regions(vec![bottom_region(8, true, false)]);
    state.compose(106, 30).expect("frame");
    let outer = state
        .hits
        .regions
        .first()
        .cloned()
        .expect("region hit")
        .outer;
    let far_edge = outer.y + outer.height;
    let layout = state.layout(106, 30);
    let tab_top = layout.tab_bar.y.min(layout.pane_surface.y);
    let tallest = far_edge - tab_top - crate::region::MIN_TAB_SURFACE_HEIGHT;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        outer.x + 5,
        outer.y,
    )]);

    assert_eq!(
        drag_border_to(&mut state, outer.x + 5, 0),
        u32::from(tallest)
    );
    assert!(shown_after_resize(
        &mut state,
        2,
        bottom_region(tallest, true, false)
    ));
    assert!(!shown_after_resize(
        &mut state,
        3,
        bottom_region(tallest + 1, true, false)
    ));
}

#[test]
fn bottom_region_bindings_target_only_the_bottom_region() {
    let mut state =
        shell_with_regions(vec![region(30, true, false), bottom_region(8, true, false)]);
    state.compose(106, 30).expect("frame");
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusBottomRegion),
        &mut outcome,
    );
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ToggleBottomRegion),
        &mut outcome,
    );
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusRegion),
        &mut outcome,
    );
    let methods = endpoint_methods(&outcome);
    assert!(
        matches!(
            &methods[..],
            [
                crate::api::schema::Method::RegionFocus(bottom_focus),
                crate::api::schema::Method::RegionHide(bottom_hide),
                crate::api::schema::Method::RegionFocus(right_focus),
            ] if bottom_focus.region_id == BOTTOM_REGION_ID
                && bottom_hide.region_id == BOTTOM_REGION_ID
                && right_focus.region_id == REGION_ID
        ),
        "{methods:?}"
    );
}

#[test]
fn focused_bottom_region_takes_keyboard_input() {
    let mut state = shell_with_regions(vec![region(30, true, false), bottom_region(8, true, true)]);
    state.compose(106, 30).expect("frame");
    assert_eq!(
        state.focused_region_terminal().as_deref(),
        Some(BOTTOM_REGION_TERMINAL)
    );
}
