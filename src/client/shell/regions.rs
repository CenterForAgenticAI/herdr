//! Client side of desktop regions: frame reservation, drawing, focus routing, mouse and the
//! border drag.
//!
//! Regions are server state. The client lays out what the active endpoint projects, and only
//! when that endpoint advertised the region capability, so a client talking to an older
//! server lays out exactly as before. While another machine is viewed, an edge that machine
//! leaves free shows this machine's region instead, when the local server advertised
//! `region_background`. That fallback region keeps a client-local focus, and its input and
//! show, hide and resize go to the local server.

use super::*;
use crate::protocol::region_surface::{
    self, ClientRegion, RegionProjection, RegionSurface, RegionViewport,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::region::RegionEdge;

/// Edges the client lays out, in layout order.
pub(super) const CLIENT_REGION_EDGES: [RegionEdge; 2] = [RegionEdge::Right, RegionEdge::Bottom];
/// Surfaces kept per endpoint. A server never projects more than one region per edge, so this
/// only bounds what a misbehaving server can make the client retain.
const MAX_REGION_SURFACES: usize = 8;

/// Region state one endpoint projected to this client.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientEndpointRegions {
    /// The endpoint advertised the region capability on this connection.
    supported: bool,
    /// The endpoint keeps serving its regions while another machine is viewed.
    background: bool,
    projection: Option<RegionProjection>,
    surfaces: HashMap<String, RegionSurface>,
    /// Last viewport reported per region, so an unchanged size is not resent.
    sent_viewports: HashMap<String, (u16, u16)>,
}

impl ClientEndpointRegions {
    fn reset(&mut self, supported: bool, background: bool) {
        *self = Self {
            supported,
            background: supported && background,
            ..Self::default()
        };
    }

    fn on_edge(&self, edge: RegionEdge) -> Option<&ClientRegion> {
        if !self.supported {
            return None;
        }
        self.projection
            .as_ref()?
            .regions
            .iter()
            .find(|region| region.edge == edge.as_str())
    }

    fn shown_on_edge(&self, edge: RegionEdge) -> Option<&ClientRegion> {
        self.on_edge(edge).filter(|region| region.visible)
    }
}

/// A region drawn in the last composed frame, kept for mouse hits.
#[derive(Clone)]
pub(super) struct RegionHit {
    pub(super) region_id: String,
    pub(super) edge: RegionEdge,
    pub(super) outer: Rect,
    pub(super) focused: bool,
    /// Drawn from the local server while another machine is viewed.
    pub(super) fallback: bool,
    /// Terminal hit for the inner area. `popup` is set so input goes to the region terminal.
    pub(super) terminal: PaneHit,
}

/// The region drawn on one edge and the endpoint that serves it.
#[derive(Clone, Copy)]
struct EdgeRegion<'a> {
    endpoint_id: &'a ClientEndpointId,
    regions: &'a ClientEndpointRegions,
    region: &'a ClientRegion,
    /// Served by the local server while another machine is viewed.
    fallback: bool,
    /// Keyboard input goes to this region.
    focused: bool,
}

impl ClientShellState {
    fn active_regions(&self) -> Option<&ClientEndpointRegions> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .map(|endpoint| &endpoint.regions)
    }

    /// The local server's regions, when they may fill edges the viewed machine leaves free.
    fn fallback_regions(&self) -> Option<(&ClientEndpointId, &ClientEndpointRegions)> {
        if self.active_endpoint_id.is_local() {
            return None;
        }
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id.is_local())
            .filter(|endpoint| endpoint.regions.supported && endpoint.regions.background)
            .map(|endpoint| (&endpoint.endpoint_id, &endpoint.regions))
    }

    /// The viewed machine's shown region on `edge`, else the local server's.
    fn edge_region(&self, edge: RegionEdge) -> Option<EdgeRegion<'_>> {
        if let Some(regions) = self.active_regions() {
            if let Some(region) = regions.shown_on_edge(edge) {
                return Some(EdgeRegion {
                    endpoint_id: &self.active_endpoint_id,
                    regions,
                    region,
                    fallback: false,
                    focused: region.focused && !self.fallback_focus_is_live(),
                });
            }
        }
        let (endpoint_id, regions) = self.fallback_regions()?;
        let region = regions.shown_on_edge(edge)?;
        Some(EdgeRegion {
            endpoint_id,
            regions,
            region,
            fallback: true,
            focused: self.fallback_region_focus.as_deref() == Some(region.region_id.as_str()),
        })
    }

    /// The client-local fallback focus names a fallback region that is drawn.
    fn fallback_focus_is_live(&self) -> bool {
        let Some(region_id) = self.fallback_region_focus.as_deref() else {
            return false;
        };
        let Some((_, regions)) = self.fallback_regions() else {
            return false;
        };
        CLIENT_REGION_EDGES.into_iter().any(|edge| {
            regions
                .shown_on_edge(edge)
                .is_some_and(|region| region.region_id == region_id)
                && self
                    .active_regions()
                    .and_then(|active| active.shown_on_edge(edge))
                    .is_none()
        })
    }

    fn active_region(&self, edge: RegionEdge) -> Option<&ClientRegion> {
        self.edge_region(edge).map(|source| source.region)
    }

    /// Whether region traffic from `endpoint_id` can change what is drawn.
    fn is_region_source(&self, endpoint_id: &ClientEndpointId) -> bool {
        endpoint_id == &self.active_endpoint_id
            || (endpoint_id.is_local() && self.fallback_regions().is_some())
    }

    /// The endpoint a region message from this shell must reach when it is not the viewed
    /// machine: region commands, and input to a fallback region terminal.
    pub(crate) fn region_message_owner(&self, message: &ClientMessage) -> Option<ClientEndpointId> {
        match message {
            ClientMessage::EndpointControl { kind, .. } if kind == region_surface::COMMAND_KIND => {
                Some(ClientEndpointId::Local)
            }
            ClientMessage::ClientShellPopupInput { terminal_id, .. } => CLIENT_REGION_EDGES
                .into_iter()
                .filter_map(|edge| self.edge_region(edge))
                .find(|source| source.fallback && &source.region.terminal_id == terminal_id)
                .map(|source| source.endpoint_id.clone()),
            _ => None,
        }
    }

    /// Sizes the drawn regions ask for.
    pub(super) fn region_requests(&self) -> crate::region::RegionSizes {
        let size = |edge| self.active_region(edge).map(|region| region.size);
        crate::region::RegionSizes {
            right: size(RegionEdge::Right),
            bottom: size(RegionEdge::Bottom),
        }
    }

    pub(crate) fn set_endpoint_regions_supported(
        &mut self,
        endpoint_id: &ClientEndpointId,
        supported: bool,
    ) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            let background = endpoint.regions.background;
            endpoint.regions.reset(supported, background);
        }
    }

    /// Record whether the endpoint advertised `region_background`. Call after
    /// [`Self::set_endpoint_regions_supported`], which clears it.
    pub(crate) fn set_endpoint_regions_background(
        &mut self,
        endpoint_id: &ClientEndpointId,
        background: bool,
    ) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.regions.background = endpoint.regions.supported && background;
        }
    }

    /// Forget an endpoint's regions until it connects and projects them again.
    pub(super) fn clear_endpoint_regions(&mut self, endpoint_id: &ClientEndpointId) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.regions.reset(false, false);
        }
    }

    /// Store a projection. Returns true when it can change what is drawn.
    pub(crate) fn set_endpoint_region_projection(
        &mut self,
        endpoint_id: &ClientEndpointId,
        projection: RegionProjection,
    ) -> bool {
        let active = self.is_region_source(endpoint_id);
        let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return false;
        };
        let regions = &mut endpoint.regions;
        if !regions.supported {
            return false;
        }
        if let Some(current) = &regions.projection {
            if current.boot_id == projection.boot_id && projection.revision <= current.revision {
                return false;
            }
        }
        regions.surfaces.retain(|region_id, _| {
            projection
                .regions
                .iter()
                .any(|region| &region.region_id == region_id && region.visible)
        });
        regions.sent_viewports.retain(|region_id, _| {
            projection
                .regions
                .iter()
                .any(|region| &region.region_id == region_id)
        });
        regions.projection = Some(projection);
        active
    }

    /// Store a region frame. Returns true when the shell should recompose.
    pub(crate) fn set_endpoint_region_surface(
        &mut self,
        endpoint_id: &ClientEndpointId,
        surface: RegionSurface,
    ) -> bool {
        let active = self.is_region_source(endpoint_id);
        let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return false;
        };
        let regions = &mut endpoint.regions;
        if !regions.supported {
            return false;
        }
        if regions.surfaces.len() >= MAX_REGION_SURFACES
            && !regions.surfaces.contains_key(&surface.region_id)
        {
            return false;
        }
        // A frame can overtake its projection on the render lane; keep it until the next
        // projection decides whether its region exists.
        regions.surfaces.insert(surface.region_id.clone(), surface);
        active
    }

    /// Terminal of the focused region, when that region is on screen.
    pub(super) fn focused_region_terminal(&self) -> Option<String> {
        let (cols, rows) = self.last_composed_size?;
        let layout = self.layout(cols, rows);
        CLIENT_REGION_EDGES.into_iter().find_map(|edge| {
            let source = self.edge_region(edge).filter(|source| source.focused)?;
            (!layout.region(edge).is_empty()).then(|| source.region.terminal_id.clone())
        })
    }

    /// Where keyboard input goes: the focused region, else the focused pane.
    pub(super) fn focused_input_target(&self) -> Option<ClientInputTarget> {
        self.focused_region_terminal()
            .map(ClientInputTarget::Popup)
            .or_else(|| self.focused_pane_id().map(ClientInputTarget::Pane))
    }

    /// Draw the region border and title into the chrome buffer.
    pub(super) fn render_region_border(&self, buffer: &mut Buffer, edge: RegionEdge, outer: Rect) {
        if outer.is_empty() {
            return;
        }
        let Some(source) = self.edge_region(edge) else {
            return;
        };
        let region = source.region;
        let palette = &self.config.palette;
        let border = if source.focused {
            palette.accent
        } else {
            palette.overlay0
        };
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(ratatui::style::Style::default().fg(border))
            .title(region.title.clone());
        ratatui::widgets::Widget::render(ratatui::widgets::Clear, outer, buffer);
        ratatui::widgets::Widget::render(block, outer, buffer);
    }

    /// Copy the region terminal into the frame and return its hit. The pane cursor stays
    /// unless the region has focus.
    pub(super) fn blit_region(
        &self,
        frame: &mut FrameData,
        edge: RegionEdge,
        outer: Rect,
    ) -> Option<RegionHit> {
        if outer.is_empty() {
            return None;
        }
        let source = self.edge_region(edge)?;
        let (fallback, focused) = (source.fallback, source.focused);
        let region = source.region.clone();
        let inner = crate::region::region_inner(outer);
        let surface = source
            .regions
            .surfaces
            .get(&region.region_id)
            .filter(|surface| surface.terminal_id == region.terminal_id);
        let pane_cursor = frame.cursor.clone();
        if let Some(surface) = surface {
            let graphics = std::mem::take(&mut frame.graphics);
            blit_pane_surface(frame, &surface.frame, inner);
            frame.graphics = graphics;
        } else {
            frame.cursor = None;
        }
        if !focused {
            frame.cursor = pane_cursor;
        }
        Some(RegionHit {
            region_id: region.region_id,
            edge,
            outer,
            focused,
            fallback,
            terminal: PaneHit {
                rect: outer,
                inner_rect: inner,
                scrollbar_rect: None,
                scroll: None,
                pane_id: region.terminal_id,
                popup: true,
                mouse_reporting: region.mouse_reporting,
                sgr_pixel_mouse: false,
                pixel_width: 0,
                pixel_height: 0,
            },
        })
    }

    /// Inner sizes of the drawn regions to report to their servers. A region the layout hides
    /// for this frame reports zero, which the server treats as not viewing it. So does a local
    /// region that the viewed machine's region covers.
    pub(super) fn region_viewports(
        &self,
        layout: &ClientShellLayout,
    ) -> Vec<(ClientEndpointId, RegionViewport)> {
        let mut viewports = Vec::new();
        for edge in CLIENT_REGION_EDGES {
            let Some(source) = self.edge_region(edge) else {
                continue;
            };
            let outer = layout.region(edge);
            let inner = if outer.is_empty() {
                Rect::default()
            } else {
                crate::region::region_inner(outer)
            };
            viewports.push((
                source.endpoint_id.clone(),
                RegionViewport {
                    region_id: source.region.region_id.clone(),
                    cols: inner.width,
                    rows: inner.height,
                },
            ));
        }
        if let Some((endpoint_id, regions)) = self.fallback_regions() {
            for edge in CLIENT_REGION_EDGES {
                let Some(region) = regions.shown_on_edge(edge) else {
                    continue;
                };
                if viewports
                    .iter()
                    .any(|(_, viewport)| viewport.region_id == region.region_id)
                {
                    continue;
                }
                viewports.push((
                    endpoint_id.clone(),
                    RegionViewport {
                        region_id: region.region_id.clone(),
                        cols: 0,
                        rows: 0,
                    },
                ));
            }
        }
        viewports
    }

    /// Next viewport message to send, skipping queued sizes equal to the last one sent.
    pub(crate) fn take_region_viewport_report(
        &mut self,
    ) -> Option<(ClientEndpointId, ClientMessage)> {
        while !self.pending_region_viewports.is_empty() {
            let (endpoint_id, viewport) = self.pending_region_viewports.remove(0);
            if let Some(report) = self.region_viewport_message(endpoint_id, viewport) {
                return Some(report);
            }
        }
        None
    }

    fn region_viewport_message(
        &mut self,
        endpoint_id: ClientEndpointId,
        viewport: RegionViewport,
    ) -> Option<(ClientEndpointId, ClientMessage)> {
        let endpoint = self
            .endpoints
            .iter_mut()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)?;
        if !endpoint.regions.supported {
            return None;
        }
        let size = (viewport.cols, viewport.rows);
        if endpoint.regions.sent_viewports.get(&viewport.region_id) == Some(&size) {
            return None;
        }
        let data = region_surface::viewport_data(&viewport).ok()?;
        endpoint
            .regions
            .sent_viewports
            .insert(viewport.region_id, size);
        Some((
            endpoint_id,
            ClientMessage::EndpointControl {
                kind: region_surface::VIEWPORT_KIND.into(),
                data,
            },
        ))
    }

    /// Handle a mouse event over the drawn region. Returns true when the region consumed it.
    pub(super) fn handle_region_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let point = (mouse.column, mouse.row);
        // Drags and releases belong to whichever gesture started them: a pane selection may
        // run across the region, and region gestures continue through `pane_mouse_gesture`.
        if matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
            return false;
        }
        let Some(hit) = self
            .hits
            .regions
            .iter()
            .find(|hit| super::contains(hit.outer, point))
            .cloned()
        else {
            // A press anywhere else takes the keyboard off a fallback region.
            if matches!(mouse.kind, MouseEventKind::Down(_))
                && self.fallback_region_focus.take().is_some()
            {
                outcome.repaint = true;
            }
            return false;
        };
        let in_terminal = super::contains(hit.terminal.inner_rect, point);
        // The inner border faces the tab area: the left side of a right region, the top of
        // a bottom region. Its far side stays put while the user drags it.
        let border = match hit.edge {
            RegionEdge::Right => {
                (mouse.column == hit.outer.x).then(|| hit.outer.x.saturating_add(hit.outer.width))
            }
            RegionEdge::Bottom => {
                (mouse.row == hit.outer.y).then(|| hit.outer.y.saturating_add(hit.outer.height))
            }
        };
        if let (MouseEventKind::Down(MouseButton::Left), Some(far_edge)) = (mouse.kind, border) {
            self.chrome_drag = Some(ClientChromeDrag::RegionBorder {
                region_id: hit.region_id,
                edge: hit.edge,
                far_edge,
                last_sent: None,
                fallback: hit.fallback,
            });
            return true;
        }
        match mouse.kind {
            MouseEventKind::Down(button) => {
                if self.selection.take().is_some() {
                    outcome.repaint = true;
                }
                if hit.fallback {
                    if !hit.focused {
                        self.fallback_region_focus = Some(hit.region_id);
                        outcome.repaint = true;
                    }
                } else {
                    if self.fallback_region_focus.take().is_some() {
                        outcome.repaint = true;
                    }
                    if !hit.focused {
                        self.push_endpoint_method(
                            crate::api::schema::Method::RegionFocus(
                                crate::api::schema::RegionTarget {
                                    region_id: hit.region_id,
                                },
                            ),
                            outcome,
                        );
                    }
                }
                if hit.terminal.mouse_reporting && in_terminal {
                    self.push_pane_mouse_event(&hit.terminal, mouse, mouse.modifiers, outcome);
                    self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                        last_position: self.pane_mouse_position(&hit.terminal, mouse),
                        hit: hit.terminal,
                        button,
                        stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                        last_event: mouse,
                    });
                }
            }
            MouseEventKind::Moved if hit.terminal.mouse_reporting && in_terminal => {
                self.push_pane_mouse_event(&hit.terminal, mouse, mouse.modifiers, outcome);
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
                if in_terminal =>
            {
                self.push_pane_mouse_event(&hit.terminal, mouse, mouse.modifiers, outcome);
            }
            _ => {}
        }
        true
    }

    /// Follow a border drag: the region spans from the pointer to its fixed far edge. The drag
    /// stops where the tab area reaches its minimum, so the region never hides mid-drag.
    pub(super) fn drag_region_border(&mut self, mouse: MouseEvent, outcome: &mut ClientShellInput) {
        // The tab area's near side does not depend on this region's size: the right region
        // takes columns after the sidebar, the bottom region takes rows below the tab bar.
        let tab_origin = self.last_composed_size.map(|(cols, rows)| {
            let layout = self.layout(cols, rows);
            (
                layout.pane_surface.x,
                layout.tab_bar.y.min(layout.pane_surface.y),
            )
        });
        let Some(ClientChromeDrag::RegionBorder {
            region_id,
            edge,
            far_edge,
            last_sent,
            fallback,
        }) = self.chrome_drag.as_mut()
        else {
            return;
        };
        let edge = *edge;
        let (pointer, near_side) = match (edge, tab_origin) {
            (RegionEdge::Right, origin) => (mouse.column, origin.map(|(x, _)| x)),
            (RegionEdge::Bottom, origin) => (mouse.row, origin.map(|(_, y)| y)),
        };
        let mut size = far_edge.saturating_sub(pointer);
        if let Some(near_side) = near_side {
            let span = far_edge.saturating_sub(near_side);
            size = size.min(crate::region::max_region_size_in(edge, span));
        }
        let size = size.max(1);
        if *last_sent == Some(size) {
            return;
        }
        *last_sent = Some(size);
        let region_id = region_id.clone();
        if *fallback {
            push_region_command(
                region_surface::RegionCommand::Resize { region_id, size },
                outcome,
            );
            return;
        }
        self.push_endpoint_method(
            crate::api::schema::Method::RegionResize(crate::api::schema::RegionResizeParams {
                region_id,
                size: u32::from(size),
            }),
            outcome,
        );
    }

    /// `toggle_region` and `toggle_bottom_region`: show the edge's region when hidden, hide
    /// it when shown.
    /// A fallback region is hidden or shown on the local server, after the viewed machine's own
    /// region on the edge.
    pub(super) fn toggle_region(&mut self, edge: RegionEdge, outcome: &mut ClientShellInput) {
        let drawn = self
            .edge_region(edge)
            .map(|source| (source.region.region_id.clone(), source.fallback));
        let (region_id, visible, fallback) = if let Some((region_id, fallback)) = drawn {
            (region_id, true, fallback)
        } else if let Some(region) = self
            .active_regions()
            .and_then(|regions| regions.on_edge(edge))
        {
            (region.region_id.clone(), region.visible, false)
        } else if let Some(region) = self
            .fallback_regions()
            .and_then(|(_, regions)| regions.on_edge(edge))
        {
            (region.region_id.clone(), region.visible, true)
        } else {
            return;
        };
        if fallback {
            let command = if visible {
                region_surface::RegionCommand::Hide { region_id }
            } else {
                region_surface::RegionCommand::Show { region_id }
            };
            push_region_command(command, outcome);
            return;
        }
        let target = crate::api::schema::RegionTarget { region_id };
        let method = if visible {
            crate::api::schema::Method::RegionHide(target)
        } else {
            crate::api::schema::Method::RegionShow(target)
        };
        self.push_endpoint_method(method, outcome);
    }

    /// `focus_region` and `focus_bottom_region`: focus the edge's shown region, or return
    /// focus to the last pane.
    pub(super) fn focus_region(&mut self, edge: RegionEdge, outcome: &mut ClientShellInput) {
        let Some((region_id, fallback, focused)) = self.edge_region(edge).map(|source| {
            (
                source.region.region_id.clone(),
                source.fallback,
                source.focused,
            )
        }) else {
            return;
        };
        if fallback {
            self.fallback_region_focus = (!focused).then_some(region_id);
            outcome.repaint = true;
            return;
        }
        if self.fallback_region_focus.take().is_some() {
            outcome.repaint = true;
        }
        let method = if focused {
            let Some(pane_id) = self.focused_pane_id() else {
                return;
            };
            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id })
        } else {
            crate::api::schema::Method::RegionFocus(crate::api::schema::RegionTarget { region_id })
        };
        self.push_endpoint_method(method, outcome);
    }
}

/// Queue a region command for the local server. The runtime routes it there, not to the
/// viewed machine.
fn push_region_command(command: region_surface::RegionCommand, outcome: &mut ClientShellInput) {
    match region_surface::command_message(&command) {
        Ok(message) => outcome.requests.push(message),
        Err(err) => tracing::warn!(err = %err, "failed to encode region command"),
    }
}
