//! Client side of desktop regions: frame reservation, drawing, focus routing, mouse and the
//! border drag.
//!
//! Regions are server state. The client only lays out what the active endpoint projects, and
//! only when that endpoint advertised the region capability, so a client talking to an older
//! server lays out exactly as before.

use super::*;
use crate::protocol::region_surface::{
    self, ClientRegion, RegionProjection, RegionSurface, RegionViewport,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

const RIGHT_EDGE: &str = "right";
/// Surfaces kept per endpoint. A server never projects more than one region per edge, so this
/// only bounds what a misbehaving server can make the client retain.
const MAX_REGION_SURFACES: usize = 8;

/// Region state one endpoint projected to this client.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientEndpointRegions {
    /// The endpoint advertised the region capability on this connection.
    supported: bool,
    projection: Option<RegionProjection>,
    surfaces: HashMap<String, RegionSurface>,
    /// Last viewport reported per region, so an unchanged size is not resent.
    sent_viewports: HashMap<String, (u16, u16)>,
}

impl ClientEndpointRegions {
    fn reset(&mut self, supported: bool) {
        *self = Self {
            supported,
            ..Self::default()
        };
    }

    fn right(&self) -> Option<&ClientRegion> {
        if !self.supported {
            return None;
        }
        self.projection
            .as_ref()?
            .regions
            .iter()
            .find(|region| region.edge == RIGHT_EDGE)
    }

    fn shown_right(&self) -> Option<&ClientRegion> {
        self.right().filter(|region| region.visible)
    }
}

/// The region drawn in the last composed frame, kept for mouse hits.
#[derive(Clone)]
pub(super) struct RegionHit {
    pub(super) region_id: String,
    pub(super) outer: Rect,
    pub(super) focused: bool,
    /// Terminal hit for the inner area. `popup` is set so input goes to the region terminal.
    pub(super) terminal: PaneHit,
}

impl ClientShellState {
    fn active_regions(&self) -> Option<&ClientEndpointRegions> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .map(|endpoint| &endpoint.regions)
    }

    fn active_right_region(&self) -> Option<&ClientRegion> {
        self.active_regions()?.shown_right()
    }

    /// Width the active endpoint's right region asks for, when one is shown.
    pub(super) fn right_region_request(&self) -> Option<u16> {
        self.active_right_region().map(|region| region.size)
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
            endpoint.regions.reset(supported);
        }
    }

    /// Forget an endpoint's regions until it connects and projects them again.
    pub(super) fn clear_endpoint_regions(&mut self, endpoint_id: &ClientEndpointId) {
        self.set_endpoint_regions_supported(endpoint_id, false);
    }

    /// Store a projection. Returns true when it changed what the active endpoint shows.
    pub(crate) fn set_endpoint_region_projection(
        &mut self,
        endpoint_id: &ClientEndpointId,
        projection: RegionProjection,
    ) -> bool {
        let active = endpoint_id == &self.active_endpoint_id;
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

    /// Store a region frame. Returns true when the active endpoint should recompose.
    pub(crate) fn set_endpoint_region_surface(
        &mut self,
        endpoint_id: &ClientEndpointId,
        surface: RegionSurface,
    ) -> bool {
        let active = endpoint_id == &self.active_endpoint_id;
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

    /// Terminal of the focused right region, when that region is on screen.
    pub(super) fn focused_region_terminal(&self) -> Option<String> {
        let region = self.active_right_region().filter(|region| region.focused)?;
        let (cols, rows) = self.last_composed_size?;
        (!self.layout(cols, rows).region.is_empty()).then(|| region.terminal_id.clone())
    }

    /// Where keyboard input goes: the focused region, else the focused pane.
    pub(super) fn focused_input_target(&self) -> Option<ClientInputTarget> {
        self.focused_region_terminal()
            .map(ClientInputTarget::Popup)
            .or_else(|| self.focused_pane_id().map(ClientInputTarget::Pane))
    }

    /// Draw the region border and title into the chrome buffer.
    pub(super) fn render_region_border(&self, buffer: &mut Buffer, outer: Rect) {
        if outer.is_empty() {
            return;
        }
        let Some(region) = self.active_right_region() else {
            return;
        };
        let palette = &self.config.palette;
        let border = if region.focused {
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
    pub(super) fn blit_region(&self, frame: &mut FrameData, outer: Rect) -> Option<RegionHit> {
        if outer.is_empty() {
            return None;
        }
        let region = self.active_right_region()?.clone();
        let inner = crate::region::region_inner(outer);
        let surface = self
            .active_regions()
            .and_then(|regions| regions.surfaces.get(&region.region_id))
            .filter(|surface| surface.terminal_id == region.terminal_id);
        let pane_cursor = frame.cursor.clone();
        if let Some(surface) = surface {
            let graphics = std::mem::take(&mut frame.graphics);
            blit_pane_surface(frame, &surface.frame, inner);
            frame.graphics = graphics;
        } else {
            frame.cursor = None;
        }
        if !region.focused {
            frame.cursor = pane_cursor;
        }
        Some(RegionHit {
            region_id: region.region_id,
            outer,
            focused: region.focused,
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

    /// Inner size of the right region to report to the server. A region the layout hides
    /// for this frame reports zero, which the server treats as not viewing it.
    pub(super) fn region_viewport(
        &self,
        outer: Rect,
    ) -> Option<(ClientEndpointId, RegionViewport)> {
        let region = self.active_right_region()?;
        let inner = if outer.is_empty() {
            Rect::default()
        } else {
            crate::region::region_inner(outer)
        };
        Some((
            self.active_endpoint_id.clone(),
            RegionViewport {
                region_id: region.region_id.clone(),
                cols: inner.width,
                rows: inner.height,
            },
        ))
    }

    /// Viewport message to send, when the queued size differs from the last one sent.
    pub(crate) fn take_region_viewport_report(
        &mut self,
    ) -> Option<(ClientEndpointId, ClientMessage)> {
        let (endpoint_id, viewport) = self.pending_region_viewport.take()?;
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
        let Some(hit) = self.hits.region.clone() else {
            return false;
        };
        let point = (mouse.column, mouse.row);
        // Drags and releases belong to whichever gesture started them: a pane selection may
        // run across the region, and region gestures continue through `pane_mouse_gesture`.
        if matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
            || !super::contains(hit.outer, point)
        {
            return false;
        }
        let in_terminal = super::contains(hit.terminal.inner_rect, point);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.column == hit.outer.x => {
                self.chrome_drag = Some(ClientChromeDrag::RegionBorder {
                    region_id: hit.region_id,
                    right_edge: hit.outer.x.saturating_add(hit.outer.width),
                    last_sent: None,
                });
            }
            MouseEventKind::Down(button) => {
                if self.selection.take().is_some() {
                    outcome.repaint = true;
                }
                if !hit.focused {
                    self.push_endpoint_method(
                        crate::api::schema::Method::RegionFocus(crate::api::schema::RegionTarget {
                            region_id: hit.region_id,
                        }),
                        outcome,
                    );
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

    /// Follow a border drag: the region spans from the pointer to its fixed right edge.
    pub(super) fn drag_region_border(&mut self, column: u16, outcome: &mut ClientShellInput) {
        let Some(ClientChromeDrag::RegionBorder {
            region_id,
            right_edge,
            last_sent,
        }) = self.chrome_drag.as_mut()
        else {
            return;
        };
        let size = right_edge.saturating_sub(column).max(1);
        if *last_sent == Some(size) {
            return;
        }
        *last_sent = Some(size);
        let region_id = region_id.clone();
        self.push_endpoint_method(
            crate::api::schema::Method::RegionResize(crate::api::schema::RegionResizeParams {
                region_id,
                size: u32::from(size),
            }),
            outcome,
        );
    }

    /// `toggle_region`: show the right region when hidden, hide it when shown.
    pub(super) fn toggle_region(&mut self, outcome: &mut ClientShellInput) {
        let Some(region) = self.active_regions().and_then(|regions| regions.right()) else {
            return;
        };
        let target = crate::api::schema::RegionTarget {
            region_id: region.region_id.clone(),
        };
        let method = if region.visible {
            crate::api::schema::Method::RegionHide(target)
        } else {
            crate::api::schema::Method::RegionShow(target)
        };
        self.push_endpoint_method(method, outcome);
    }

    /// `focus_region`: focus the shown right region, or return focus to the last pane.
    pub(super) fn focus_region(&mut self, outcome: &mut ClientShellInput) {
        let Some(region) = self.active_right_region() else {
            return;
        };
        let method = if region.focused {
            let Some(pane_id) = self.focused_pane_id() else {
                return;
            };
            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id })
        } else {
            crate::api::schema::Method::RegionFocus(crate::api::schema::RegionTarget {
                region_id: region.region_id.clone(),
            })
        };
        self.push_endpoint_method(method, outcome);
    }
}
