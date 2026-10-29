//! Runtime half of desktop regions: spawning, closing, focus, size, and visibility.
//!
//! The pure bookkeeping lives in `crate::region`. A region terminal is registered in
//! `terminal_runtimes` and `state.terminals` like the popup's, and never attached to a pane
//! in any workspace layout.

use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, RegionCloseReason, RegionExitInfo, RegionInfo,
};
use crate::app::state::{AppState, ToastKind, ToastNotification};
use crate::app::App;
use crate::layout::PaneId;
use crate::pane::PaneLaunchEnv;
use crate::region::{
    RegionEdge, RegionId, RegionInstance, RegionScope, RegionSizeBounds, DEFAULT_REGION_SIZE,
};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

/// Everything `open_region` needs, already validated by the API layer.
pub(crate) struct RegionLaunch {
    pub plugin_id: String,
    pub plugin_name: String,
    pub entrypoint: String,
    pub title: String,
    pub edge: RegionEdge,
    pub scope: RegionScope,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub manifest_size: Option<u16>,
    pub bounds: RegionSizeBounds,
    /// Size the caller asked for. Wider than `u16` so a hostile value still clamps.
    pub requested_size: Option<u32>,
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegionOpenError {
    EdgeDisabled(RegionEdge),
    SlotOccupied {
        edge: RegionEdge,
        scope: RegionScope,
        occupant: RegionId,
    },
    Spawn(String),
}

impl RegionOpenError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::EdgeDisabled(_) => "region_edge_disabled",
            Self::SlotOccupied { .. } => "region_slot_occupied",
            Self::Spawn(_) => "region_open_failed",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::EdgeDisabled(edge) => format!(
                "regions on the {} edge are disabled by [ui.regions.{}].enabled",
                edge.as_str(),
                edge.as_str()
            ),
            Self::SlotOccupied {
                edge,
                scope,
                occupant,
            } => format!(
                "the {} edge in {} scope already holds region {occupant}",
                edge.as_str(),
                scope.as_str()
            ),
            Self::Spawn(err) => format!("failed to start region command: {err}"),
        }
    }
}

/// Result of a region operation that names a region by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegionLookupError {
    NotFound,
    Hidden,
}

impl RegionLookupError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "region_not_found",
            Self::Hidden => "region_hidden",
        }
    }

    pub(crate) fn message(&self, region_id: &str) -> String {
        match self {
            Self::NotFound => format!("region {region_id} not found"),
            Self::Hidden => format!("region {region_id} is hidden; show it before focusing it"),
        }
    }
}

/// Clamp a caller-supplied size into the region's bounds. Values wider than `u16` saturate
/// first, so no request can reach zero or exceed the maximum.
pub(crate) fn clamp_requested_size(bounds: RegionSizeBounds, size: u32) -> u16 {
    bounds.clamp(u16::try_from(size).unwrap_or(u16::MAX))
}

impl AppState {
    pub(crate) fn region_info(&self, region: &RegionInstance) -> RegionInfo {
        RegionInfo {
            region_id: region.region_id.to_string(),
            plugin_id: region.plugin_id.clone(),
            entrypoint: region.entrypoint.clone(),
            title: region.title.clone(),
            edge: region.edge,
            scope: region.scope,
            size: region.size,
            min_size: region.bounds.min(),
            max_size: region.bounds.max(),
            visible: region.visible,
            focused: self.effective_region_focus() == Some(&region.region_id),
        }
    }

    pub(crate) fn region_info_by_id(&self, region_id: &str) -> Option<RegionInfo> {
        self.regions
            .get_by_id(region_id)
            .map(|region| self.region_info(region))
    }

    pub(crate) fn region_infos(&self) -> Vec<RegionInfo> {
        self.regions
            .iter()
            .map(|region| self.region_info(region))
            .collect()
    }

    /// Region that holds focus right now. Focus that a pane focus change has superseded, or
    /// that names a closed or hidden region, is inert even before `reconcile_region_focus`
    /// runs.
    pub(crate) fn effective_region_focus(&self) -> Option<&RegionId> {
        let focus = self.region_focus.as_ref()?;
        if self.region_focus_anchor != self.current_pane_focus_target() {
            return None;
        }
        self.regions
            .get_by_id(focus.as_str())
            .filter(|region| region.visible)
            .map(|region| &region.region_id)
    }

    /// Terminal that receives input while a region holds focus.
    #[cfg(test)]
    pub(crate) fn focused_region_terminal(&self) -> Option<&TerminalId> {
        let focus = self.effective_region_focus()?;
        self.regions
            .get_by_id(focus.as_str())
            .map(|region| &region.terminal_id)
    }

    /// Drop region focus. Returns whether focus was held.
    pub(crate) fn clear_region_focus(&mut self) -> bool {
        self.region_focus_anchor = None;
        self.region_focus.take().is_some()
    }

    /// Keep region focus consistent: it must name a live, visible region, and any pane focus
    /// change since the region took focus releases it.
    pub(crate) fn reconcile_region_focus(&mut self) -> bool {
        let Some(focus) = self.region_focus.as_ref() else {
            return false;
        };
        let region_live = self
            .regions
            .get_by_id(focus.as_str())
            .is_some_and(|region| region.visible);
        if !region_live || self.region_focus_anchor != self.current_pane_focus_target() {
            return self.clear_region_focus();
        }
        false
    }

    fn resolve_open_size(&self, launch: &RegionLaunch) -> u16 {
        let configured = match launch.edge {
            RegionEdge::Right => self.regions_config.right.size,
            RegionEdge::Bottom => self.regions_config.bottom.size,
        };
        let size = launch
            .requested_size
            .or(configured.map(u32::from))
            .or(launch.manifest_size.map(u32::from))
            .unwrap_or(u32::from(DEFAULT_REGION_SIZE));
        clamp_requested_size(launch.bounds, size)
    }

    fn region_edge_enabled(&self, edge: RegionEdge) -> bool {
        match edge {
            RegionEdge::Right => self.regions_config.right.enabled,
            RegionEdge::Bottom => self.regions_config.bottom.enabled,
        }
    }

    fn region_edge_starts_hidden(&self, edge: RegionEdge) -> bool {
        match edge {
            RegionEdge::Right => self.regions_config.right.start_hidden,
            RegionEdge::Bottom => self.regions_config.bottom.start_hidden,
        }
    }

    /// Initial terminal size, as (rows, cols), before any client reports the region's inner
    /// rect. `size` runs across the edge; the other side spans the current terminal area.
    fn initial_region_terminal_size(&self, edge: RegionEdge, size: u16) -> (u16, u16) {
        let area = if self.view.terminal_area.height >= 4 {
            (
                self.view.terminal_area.width,
                self.view.terminal_area.height,
            )
        } else {
            self.headless_size
        };
        let (cols, rows) = match edge {
            RegionEdge::Right => (size, area.1),
            RegionEdge::Bottom => (area.0, size),
        };
        (rows.saturating_sub(2).max(1), cols.saturating_sub(2).max(1))
    }
}

impl App {
    fn emit_region_event(&mut self, event: EventKind, data: EventData) {
        self.emit_event(EventEnvelope { event, data });
    }

    fn region_changed(&mut self) {
        self.state.mark_session_dirty();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Spawn a region terminal and claim its slot. Refuses an occupied slot without touching
    /// the occupant.
    pub(crate) fn open_region(
        &mut self,
        launch: RegionLaunch,
    ) -> Result<RegionInfo, RegionOpenError> {
        if !self.state.region_edge_enabled(launch.edge) {
            return Err(RegionOpenError::EdgeDisabled(launch.edge));
        }
        if let Some(occupant) = self.state.regions.get(launch.edge) {
            return Err(RegionOpenError::SlotOccupied {
                edge: launch.edge,
                scope: launch.scope,
                occupant: occupant.region_id.clone(),
            });
        }
        let size = self.state.resolve_open_size(&launch);
        let (rows, cols) = self.state.initial_region_terminal_size(launch.edge, size);
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let launch_env = PaneLaunchEnv::from_extra(launch.env).without_pane_identity();
        let runtime = TerminalRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            launch.cwd.clone(),
            &launch.argv,
            &launch_env,
            crate::pane::AgentDetection::Disabled,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.pane_host_terminal_appearance(),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )
        .map_err(|err| RegionOpenError::Spawn(err.to_string()))?;
        let mut terminal =
            TerminalState::new(terminal_id.clone(), launch.cwd).with_launch_argv(launch.argv);
        terminal.set_manual_label(launch.title.clone());
        let region = RegionInstance {
            region_id: RegionId::alloc(),
            plugin_id: launch.plugin_id,
            entrypoint: launch.entrypoint,
            terminal_id: terminal_id.clone(),
            pane_id,
            edge: launch.edge,
            scope: launch.scope,
            title: launch.title,
            size,
            bounds: launch.bounds,
            visible: !self.state.region_edge_starts_hidden(launch.edge),
        };
        tracing::debug!(
            region = %region.region_id,
            plugin = %region.plugin_id,
            plugin_name = %launch.plugin_name,
            "opening region"
        );
        let region_id = region.region_id.clone();
        let visible = region.visible;
        self.state
            .regions
            .insert(region)
            .map_err(|occupied| RegionOpenError::SlotOccupied {
                edge: occupied.edge,
                scope: occupied.scope,
                occupant: occupied.occupant,
            })?;
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id, terminal);
        if launch.focus && visible {
            self.focus_region_unchecked(region_id.clone());
        }
        self.region_changed();
        let info = self
            .state
            .region_info_by_id(region_id.as_str())
            .ok_or_else(|| RegionOpenError::Spawn("region disappeared after open".into()))?;
        self.emit_region_event(
            EventKind::RegionOpened,
            EventData::RegionOpened {
                region: info.clone(),
            },
        );
        Ok(info)
    }

    /// Remove a region, kill its process, and emit `region.closed`. Returns the closed
    /// region's last info, or `None` when no such region exists.
    pub(crate) fn close_region(
        &mut self,
        region_id: &str,
        reason: RegionCloseReason,
        exit: Option<RegionExitInfo>,
    ) -> Option<RegionInfo> {
        let info = self.state.region_info_by_id(region_id)?;
        let region = self.state.regions.remove_by_id(region_id)?;
        if self.state.region_focus.as_ref() == Some(&region.region_id) {
            self.state.clear_region_focus();
        }
        self.release_region_terminal(region.terminal_id);
        self.region_changed();
        let mut info = info;
        info.focused = false;
        self.emit_region_event(
            EventKind::RegionClosed,
            EventData::RegionClosed {
                region: info.clone(),
                reason,
                exit,
            },
        );
        Some(info)
    }

    fn release_region_terminal(&mut self, terminal_id: TerminalId) {
        self.state.direct_attach_resize_locks.remove(&terminal_id);
        self.state.terminals.remove(&terminal_id);
        self.shutdown_terminal_runtime(terminal_id);
    }

    /// Close every region a plugin owns. Used when the plugin is disabled or removed.
    pub(crate) fn close_plugin_regions(&mut self, plugin_id: &str, reason: RegionCloseReason) {
        let region_ids: Vec<String> = self
            .state
            .regions
            .iter()
            .filter(|region| region.plugin_id == plugin_id)
            .map(|region| region.region_id.to_string())
            .collect();
        for region_id in region_ids {
            self.close_region(&region_id, reason, None);
        }
    }

    /// Handle a region process exit. Returns `false` when `pane_id` is not a region.
    pub(crate) fn handle_region_process_exit(&mut self, pane_id: PaneId) -> bool {
        let Some(region) = self.state.regions.find_by_pane(pane_id) else {
            return false;
        };
        let region_id = region.region_id.to_string();
        let plugin_id = region.plugin_id.clone();
        let title = region.title.clone();
        let status = self
            .terminal_runtimes
            .get(&region.terminal_id)
            .and_then(TerminalRuntime::child_exit_status);
        let exit = status.as_ref().map(|status| RegionExitInfo {
            code: status.code,
            signal: status.signal.clone(),
        });
        self.close_region(&region_id, RegionCloseReason::Exited, exit);
        if let Some(status) = status.filter(|status| !status.success()) {
            let detail = match &status.signal {
                Some(signal) => format!("killed by {signal}"),
                None => format!("exit status {}", status.code),
            };
            self.raise_region_exit_toast(&plugin_id, &title, &detail);
        }
        true
    }

    fn raise_region_exit_toast(&mut self, plugin_id: &str, title: &str, detail: &str) {
        if !matches!(
            self.state.toast_config.delivery,
            crate::config::ToastDelivery::Herdr
        ) {
            tracing::info!(plugin = plugin_id, detail, "region process failed");
            return;
        }
        let previous = self.state.toast.clone();
        self.state.toast = Some(ToastNotification {
            kind: ToastKind::NeedsAttention,
            title: format!("{title} ({plugin_id}) closed"),
            context: detail.to_string(),
            position: None,
            target: None,
        });
        self.sync_toast_deadline(previous);
    }

    fn focus_region_unchecked(&mut self, region_id: RegionId) {
        self.state.region_focus_anchor = self.state.current_pane_focus_target();
        self.state.region_focus = Some(region_id);
    }

    /// Give keyboard focus to a visible region.
    pub(crate) fn focus_region(
        &mut self,
        region_id: &str,
    ) -> Result<RegionInfo, RegionLookupError> {
        let region = self
            .state
            .regions
            .get_by_id(region_id)
            .ok_or(RegionLookupError::NotFound)?;
        if !region.visible {
            return Err(RegionLookupError::Hidden);
        }
        let id = region.region_id.clone();
        let changed = self.state.region_focus.as_ref() != Some(&id);
        self.focus_region_unchecked(id);
        let info = self
            .state
            .region_info_by_id(region_id)
            .ok_or(RegionLookupError::NotFound)?;
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            self.emit_region_event(
                EventKind::RegionFocused,
                EventData::RegionFocused {
                    region: info.clone(),
                },
            );
        }
        Ok(info)
    }

    /// Set a region's size, clamped to its bounds.
    pub(crate) fn resize_region(
        &mut self,
        region_id: &str,
        size: u32,
    ) -> Result<RegionInfo, RegionLookupError> {
        let region = self
            .state
            .regions
            .get_mut_by_id(region_id)
            .ok_or(RegionLookupError::NotFound)?;
        let applied = clamp_requested_size(region.bounds, size);
        let changed = region.size != applied;
        region.size = applied;
        let info = self
            .state
            .region_info_by_id(region_id)
            .ok_or(RegionLookupError::NotFound)?;
        if changed {
            self.region_changed();
            self.emit_region_event(
                EventKind::RegionResized,
                EventData::RegionResized {
                    region: info.clone(),
                },
            );
        }
        Ok(info)
    }

    /// Show or hide a region. Hiding releases region focus.
    pub(crate) fn set_region_visible(
        &mut self,
        region_id: &str,
        visible: bool,
    ) -> Result<RegionInfo, RegionLookupError> {
        let region = self
            .state
            .regions
            .get_mut_by_id(region_id)
            .ok_or(RegionLookupError::NotFound)?;
        let changed = region.visible != visible;
        region.visible = visible;
        if !visible {
            self.state.reconcile_region_focus();
        }
        let info = self
            .state
            .region_info_by_id(region_id)
            .ok_or(RegionLookupError::NotFound)?;
        if changed {
            self.region_changed();
            self.emit_region_event(
                EventKind::RegionVisibilityChanged,
                EventData::RegionVisibilityChanged {
                    region: info.clone(),
                },
            );
        }
        Ok(info)
    }

    /// Resize a region terminal to the inner rect a client reported. Terminals under a direct
    /// attach resize lock keep their size.
    pub(crate) fn resize_region_terminal(&mut self, region_id: &str, cols: u16, rows: u16) -> bool {
        let Some(region) = self.state.regions.get_by_id(region_id) else {
            return false;
        };
        if cols == 0 || rows == 0 {
            return false;
        }
        if self
            .state
            .direct_attach_resize_locks
            .contains(&region.terminal_id)
        {
            return false;
        }
        let Some(runtime) = self.terminal_runtimes.get(&region.terminal_id) else {
            return false;
        };
        if runtime.current_size() == (rows, cols) {
            return false;
        }
        let cell = self.state.host_cell_size;
        runtime.resize(rows, cols, cell.width_px, cell.height_px);
        true
    }
}

impl App {
    /// Reopen regions recorded in a session snapshot. Each one respawns from its plugin's
    /// current manifest, so a plugin that is gone, disabled or no longer declares the
    /// entrypoint drops its region. Restored regions keep their size (clamped to the current
    /// bounds) and visibility, and are never focused.
    pub(crate) fn restore_regions(&mut self, snapshots: &[crate::persist::RegionSnapshot]) {
        for snapshot in snapshots {
            let launch = match self.resolve_region_launch(
                &snapshot.plugin_id,
                &snapshot.entrypoint,
                std::collections::HashMap::new(),
                Some(u32::from(snapshot.size)),
                false,
            ) {
                Ok(launch) => launch,
                Err((code, message)) => {
                    tracing::debug!(
                        plugin = %snapshot.plugin_id,
                        entrypoint = %snapshot.entrypoint,
                        code = %code,
                        message = %message,
                        "dropping restored region"
                    );
                    continue;
                }
            };
            if launch.edge != snapshot.edge || launch.scope != snapshot.scope {
                tracing::debug!(
                    plugin = %snapshot.plugin_id,
                    entrypoint = %snapshot.entrypoint,
                    "dropping restored region: manifest moved it to another slot"
                );
                continue;
            }
            match self.open_region(launch) {
                Ok(info) => {
                    if let Some(region) = self.state.regions.get_mut_by_id(&info.region_id) {
                        region.visible = snapshot.visible;
                    }
                }
                Err(err) => tracing::debug!(
                    plugin = %snapshot.plugin_id,
                    entrypoint = %snapshot.entrypoint,
                    code = err.code(),
                    "dropping restored region"
                ),
            }
        }
        self.state.reconcile_region_focus();
    }
}

#[cfg(test)]
impl App {
    /// Install a region backed by a test runtime, without spawning a process.
    pub(crate) fn install_test_region(
        &mut self,
        plugin_id: &str,
        runtime: TerminalRuntime,
    ) -> RegionId {
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let region_id = RegionId::alloc();
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/region")),
        );
        self.state
            .regions
            .insert(RegionInstance {
                region_id: region_id.clone(),
                plugin_id: plugin_id.to_string(),
                entrypoint: "board".to_string(),
                terminal_id,
                pane_id,
                edge: RegionEdge::Right,
                scope: RegionScope::Session,
                title: "Test Region".to_string(),
                size: DEFAULT_REGION_SIZE,
                bounds: RegionSizeBounds::default(),
                visible: true,
            })
            .expect("test region slot is free");
        region_id
    }
}
