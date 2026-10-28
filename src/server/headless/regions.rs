//! Desktop region delivery to region-capable client shells.
//!
//! Region traffic rides `EndpointControl` kinds from [`crate::protocol::region_surface`] and
//! goes only to shells whose hello set `regions`. Other shells never see a region.

use super::*;
use crate::protocol::region_surface::{
    self, ClientRegion, RegionProjection, RegionSurface, RegionViewport,
};
use crate::server::clients::{RegionFrameKey, RegionViewportReport, SentRegionProjection};

/// Viewport reports kept per client. Phase 1 has one region, so this only bounds a hostile
/// client that reports ids of regions that come and go.
const MAX_REGION_VIEWPORTS: usize = 8;

impl HeadlessServer {
    /// Whether any active shell with a writer opted into regions.
    pub(super) fn has_region_viewer(&self) -> bool {
        self.clients.values().any(|client| {
            client.shell_regions.is_some()
                && client.writer.is_some()
                && client.is_active_shell_client()
        })
    }

    fn client_regions(&self) -> Vec<ClientRegion> {
        self.app
            .state
            .regions
            .iter()
            .map(|region| {
                let info = self.app.state.region_info(region);
                let mouse_reporting = self
                    .app
                    .terminal_runtimes
                    .get(&region.terminal_id)
                    .is_some_and(|runtime| runtime.mouse_reporting_enabled());
                ClientRegion {
                    region_id: info.region_id,
                    terminal_id: region.terminal_id.to_string(),
                    plugin_id: info.plugin_id,
                    entrypoint: info.entrypoint,
                    title: info.title,
                    edge: info.edge.as_str().to_owned(),
                    scope: info.scope.as_str().to_owned(),
                    size: info.size,
                    min_size: info.min_size,
                    max_size: info.max_size,
                    visible: info.visible,
                    focused: info.focused,
                    mouse_reporting,
                }
            })
            .collect()
    }

    /// Record the inner size one client draws a region at, then resize region terminals.
    pub(super) fn handle_region_viewport(
        &mut self,
        client_id: u64,
        viewport: RegionViewport,
    ) -> bool {
        if self.handoff_in_progress
            || self
                .app
                .state
                .regions
                .get_by_id(&viewport.region_id)
                .is_none()
        {
            return false;
        }
        let stamp = self.allocate_activity_stamp();
        let regions = &self.app.state.regions;
        let Some(stream) = self
            .clients
            .get_mut(&client_id)
            .and_then(|client| client.shell_regions.as_mut())
        else {
            return false;
        };
        if !stream.viewports.contains_key(&viewport.region_id) {
            stream
                .viewports
                .retain(|region_id, _| regions.get_by_id(region_id).is_some());
            if stream.viewports.len() >= MAX_REGION_VIEWPORTS {
                return false;
            }
        }
        stream.viewports.insert(
            viewport.region_id,
            RegionViewportReport {
                cols: viewport.cols,
                rows: viewport.rows,
                stamp,
            },
        );
        self.apply_region_viewports()
    }

    /// The size that drives one region terminal: the foreground client's report, else the
    /// most recent report. A client that hides the region (zero size) never drives it.
    pub(super) fn driving_region_viewport(&self, region_id: &str) -> Option<(u16, u16)> {
        let usable = |client: &ClientConnection| {
            if !client.is_active_shell_client() {
                return None;
            }
            client
                .shell_regions
                .as_ref()?
                .viewports
                .get(region_id)
                .filter(|report| report.cols > 0 && report.rows > 0)
                .copied()
        };
        if let Some(report) = self
            .foreground_client_id
            .and_then(|client_id| self.clients.get(&client_id))
            .and_then(usable)
        {
            return Some((report.cols, report.rows));
        }
        self.clients
            .values()
            .filter_map(usable)
            .max_by_key(|report| report.stamp)
            .map(|report| (report.cols, report.rows))
    }

    fn apply_region_viewports(&mut self) -> bool {
        let region_ids = self
            .app
            .state
            .regions
            .iter()
            .map(|region| region.region_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let mut changed = false;
        for region_id in region_ids {
            if let Some((cols, rows)) = self.driving_region_viewport(&region_id) {
                changed |= self.app.resize_region_terminal(&region_id, cols, rows);
            }
        }
        changed
    }

    /// Send changed region projections and frames to every region-capable shell. Frames use
    /// the render slot, so a slow client keeps at most one pending frame and catches up when
    /// its writer drains.
    pub(super) fn stream_region_surfaces(&mut self) {
        let recipients = self
            .clients
            .iter()
            .filter(|(_, client)| {
                client.shell_regions.is_some()
                    && client.writer.is_some()
                    && client.is_shell_client()
            })
            .map(|(&client_id, _)| client_id)
            .collect::<Vec<_>>();
        if recipients.is_empty() {
            return;
        }
        self.apply_region_viewports();
        let current = SentRegionProjection {
            regions: self.client_regions(),
            focused: self
                .app
                .state
                .effective_region_focus()
                .map(|region_id| region_id.as_str().to_owned()),
        };
        let mut broken = Vec::new();
        for &client_id in &recipients {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            let (Some(stream), Some(writer)) =
                (client.shell_regions.as_mut(), client.writer.as_ref())
            else {
                continue;
            };
            if stream.sent_projection.as_ref() == Some(&current) {
                continue;
            }
            let revision = stream.projection_revision.saturating_add(1);
            let projection = RegionProjection {
                boot_id: self.client_shell_boot_id.clone(),
                revision,
                regions: current.regions.clone(),
                focused: current.focused.clone(),
            };
            let framed = match region_surface::projection_message(&projection)
                .map_err(|err| err.to_string())
                .and_then(|message| {
                    Self::frame_server_message(&message).map_err(|err| err.to_string())
                }) {
                Ok(framed) => framed,
                Err(err) => {
                    warn!(client_id, err = %err, "failed to encode region projection");
                    continue;
                }
            };
            if writer.control.send(framed).is_err() {
                broken.push(client_id);
                continue;
            }
            stream.projection_revision = revision;
            let live = |region_id: &String| {
                current
                    .regions
                    .iter()
                    .any(|region| &region.region_id == region_id)
            };
            stream.viewports.retain(|region_id, _| live(region_id));
            stream.sent_frames.retain(|region_id, _| live(region_id));
            stream.sent_projection = Some(current.clone());
        }

        for region in self
            .app
            .state
            .regions
            .iter()
            .filter(|region| region.visible)
        {
            let Some(runtime) = self.app.terminal_runtimes.get(&region.terminal_id) else {
                continue;
            };
            let (rows, cols) = runtime.current_size();
            let content_seq = runtime.content_seq();
            let scroll_offset = runtime
                .scroll_metrics()
                .map_or(0, |metrics| metrics.offset_from_bottom);
            let region_id = region.region_id.as_str();
            let key_for = |client: &ClientConnection| RegionFrameKey {
                surface_revision: client.shell_projection_revision,
                content_seq,
                size: (cols, rows),
                scroll_offset,
            };
            let pending = recipients
                .iter()
                .copied()
                .filter(|client_id| {
                    self.clients.get(client_id).is_some_and(|client| {
                        client.is_active_shell_client()
                            && client.shell_regions.as_ref().is_some_and(|stream| {
                                stream.sent_frames.get(region_id) != Some(&key_for(client))
                            })
                    })
                })
                .collect::<Vec<_>>();
            if pending.is_empty() || runtime.synchronized_output_active() {
                continue;
            }
            let area = Rect::new(0, 0, cols, rows);
            let (buffer, cursor) =
                crate::server::render_stream::render_terminal_virtual(runtime, area);
            let hyperlinks = runtime.visible_hyperlinks(area);
            let surface = RegionSurface {
                region_id: region_id.to_owned(),
                terminal_id: region.terminal_id.to_string(),
                frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks),
            };
            let framed = match region_surface::surface_message(&surface).and_then(|message| {
                Self::frame_server_message(&message).map_err(|err| err.to_string())
            }) {
                Ok(framed) => framed,
                Err(err) => {
                    warn!(region_id, err = %err, "failed to encode region surface");
                    continue;
                }
            };
            for client_id in pending {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    continue;
                };
                let key = key_for(client);
                let Some(writer) = client.writer.as_ref() else {
                    continue;
                };
                match writer.render.try_send(framed.clone()) {
                    Ok(()) => {
                        if let Some(stream) = client.shell_regions.as_mut() {
                            stream.sent_frames.insert(region_id.to_owned(), key);
                        }
                    }
                    Err(std::sync::mpsc::TrySendError::Full(_)) => client.defer_full_render(),
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => broken.push(client_id),
                }
            }
        }

        broken.sort_unstable();
        broken.dedup();
        for client_id in broken {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    /// Forward one input batch to a region terminal. Only a region-capable shell may type
    /// into a region, and a hidden region accepts only releases of presses it already took.
    pub(super) fn handle_region_input(
        &mut self,
        client_id: u64,
        terminal_id: &str,
        mut events: Vec<protocol::ClientPaneInputEvent>,
    ) -> bool {
        let Some((region_terminal_id, region_pane_id, visible)) = self
            .app
            .state
            .regions
            .find_by_terminal(terminal_id)
            .map(|region| (region.terminal_id.clone(), region.pane_id, region.visible))
        else {
            return false;
        };
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        if client.shell_regions.is_none() || !client.is_active_shell_client() {
            return false;
        }
        let pixel_mouse = client.pixel_mouse && client.host_sgr_pixels_active == Some(true);
        if !visible {
            events.retain(client_pane_input_releases_press);
            if events.is_empty() {
                return false;
            }
        }
        let Some(runtime) = self.app.terminal_runtimes.get(&region_terminal_id) else {
            return false;
        };
        crate::server::pane_input::downgrade_ineligible_pixel_mouse(
            &mut events,
            pixel_mouse,
            runtime.current_size(),
            runtime.pixel_size(),
        );
        let interaction = client_pane_input_has_interaction(&events);
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.track_shell_input(
                ClientShellInputTarget::Popup(terminal_id.to_owned()),
                &events,
            );
        }
        let foreground_changed = interaction && self.promote_client_to_foreground(client_id);
        let Some(runtime) = self.app.terminal_runtimes.get(&region_terminal_id) else {
            return foreground_changed;
        };
        let scroll_before = runtime.scroll_metrics();
        let mut keyboard_forwarded = false;
        let input_result =
            apply_client_terminal_input_events(runtime, &events, false, &mut keyboard_forwarded);
        let scroll_changed = runtime.scroll_metrics() != scroll_before;
        if keyboard_forwarded {
            self.note_interactive_input(region_pane_id, Instant::now());
        }
        if let Err(err) = input_result {
            warn!(client_id, terminal_id, err = %err, "region input failed");
        }
        foreground_changed || scroll_changed
    }
}
