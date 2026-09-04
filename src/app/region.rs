//! Region lifecycle: spawn, focus, and teardown.
//!
//! This is the runtime half of desktop regions, mirroring `crate::app::popup`.
//! Declarative slot state lives in `AppState::regions`; the `TerminalRuntime`
//! lives in the shared runtime registry, so state stays separate from runtime.

use crate::api::schema::{InstalledPluginInfo, PluginManifestRegion, RegionOpenParams};
use crate::app::App;
use crate::layout::PaneId;
use crate::pane::PaneLaunchEnv;
use crate::region::{RegionId, RegionInstance, RegionSizeBounds};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

impl App {
    /// Spawn a region's program and claim its slot.
    ///
    /// The caller has already verified the plugin owns this entrypoint and that
    /// the slot is free; this function performs no policy of its own beyond
    /// clamping the size through `bounds`.
    pub(super) fn open_region(
        &mut self,
        plugin: &InstalledPluginInfo,
        manifest: &PluginManifestRegion,
        bounds: RegionSizeBounds,
        requested_size: u16,
        params: RegionOpenParams,
    ) -> std::io::Result<RegionId> {
        let context = self.current_plugin_context("plugin-region");
        let cwd = self.plugin_pane_cwd(plugin, params.cwd);
        // Reuses the plugin pane projection, so a caller-supplied env map cannot
        // overwrite a Herdr-owned variable.
        let extra_env = self
            .plugin_pane_launch_env(plugin, &manifest.id, &cwd, params.env, &context)
            .map_err(|(code, message)| std::io::Error::other(format!("{code}: {message}")))?;

        let size = bounds.clamp(requested_size);
        // The region has no rectangle until the next compute_view, so start the
        // program at a sane size derived from the current frame.
        let (rows, _) = self.state.estimate_pane_size();
        let cols = size.saturating_sub(2).max(1);
        let rows = rows.max(1);

        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let launch_env = PaneLaunchEnv::from_extra(extra_env).without_pane_identity();
        let runtime = TerminalRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd.clone(),
            &manifest.command,
            &launch_env,
            crate::pane::AgentDetection::Disabled,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;

        let terminal =
            TerminalState::new(terminal_id.clone(), cwd).with_launch_argv(manifest.command.clone());
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_manual_label(manifest.title.clone());
        }

        let region_id = RegionId::alloc();
        let visible = !self.state.regions_config.right.start_hidden;
        let instance = RegionInstance::new(
            region_id.clone(),
            plugin.plugin_id.clone(),
            manifest.id.clone(),
            pane_id,
            terminal_id.clone(),
            manifest.anchor,
            manifest.scope,
            manifest.title.clone(),
            size,
            bounds,
            visible,
        );
        if self.state.regions.insert(instance).is_err() {
            // The slot was checked before spawning; losing the race means the
            // program we just started has nowhere to live.
            self.state.terminals.remove(&terminal_id);
            self.shutdown_terminal_runtime(terminal_id);
            return Err(std::io::Error::other("region slot occupied"));
        }

        if params.focus && visible {
            self.state.region_focus = Some(region_id.to_string());
        }
        self.state.mark_session_dirty();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        Ok(region_id)
    }

    /// Respawn regions recorded in the session snapshot.
    ///
    /// Slot, anchor, scope, size, and visibility are restored; the program is
    /// respawned like a restored pane shell, with no attempt to restore its
    /// internal state. A snapshot entry whose plugin is gone, disabled, or no
    /// longer declares that entrypoint is dropped rather than resurrected, so a
    /// stale slot cannot outlive its provider.
    pub(super) fn restore_regions(&mut self, snapshots: Vec<crate::persist::RegionSnapshot>) {
        if snapshots.is_empty() {
            return;
        }
        for snapshot in snapshots {
            let Some(plugin) = self
                .state
                .installed_plugins
                .get(&snapshot.plugin_id)
                .cloned()
            else {
                tracing::debug!(
                    plugin_id = %snapshot.plugin_id,
                    "dropping persisted region: plugin is no longer installed"
                );
                continue;
            };
            if !plugin.enabled {
                tracing::debug!(
                    plugin_id = %snapshot.plugin_id,
                    "dropping persisted region: plugin is disabled"
                );
                continue;
            }
            let Some(manifest) = plugin
                .regions
                .iter()
                .find(|region| region.id == snapshot.entrypoint)
                .cloned()
            else {
                tracing::debug!(
                    plugin_id = %snapshot.plugin_id,
                    entrypoint = %snapshot.entrypoint,
                    "dropping persisted region: entrypoint is no longer declared"
                );
                continue;
            };
            // The manifest still owns the hard bounds; the persisted size is
            // only a request, clamped like any other.
            let bounds = RegionSizeBounds::new(manifest.min_size, manifest.max_size);
            let params = RegionOpenParams {
                plugin_id: snapshot.plugin_id.clone(),
                entrypoint: snapshot.entrypoint.clone(),
                size: Some(snapshot.size),
                cwd: None,
                focus: false,
                env: std::collections::HashMap::new(),
            };
            match self.open_region(&plugin, &manifest, bounds, snapshot.size, params) {
                Ok(region_id) => {
                    // Visibility is part of the persisted slot, and open_region
                    // applies the config default instead.
                    if let Some(region) = self.state.regions.get_by_id_mut(region_id.as_str()) {
                        region.visible = snapshot.visible;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        plugin_id = %snapshot.plugin_id,
                        entrypoint = %snapshot.entrypoint,
                        err = %err,
                        "failed to respawn a persisted region"
                    );
                }
            }
        }
    }

    /// Close a region, release its slot, and shut its program down.
    pub(crate) fn close_region(&mut self, region_id: &str) -> bool {
        let Some(region) = self.state.regions.remove_by_id(region_id) else {
            return false;
        };
        self.release_region_runtime(&region);
        self.state.mark_session_dirty();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    /// Close every region owned by one plugin, for disable or unlink.
    pub(crate) fn close_regions_for_plugin(&mut self, plugin_id: &str) -> usize {
        let closed = self.state.regions.remove_by_plugin(plugin_id);
        if closed.is_empty() {
            return 0;
        }
        for region in &closed {
            self.release_region_runtime(region);
        }
        self.state.mark_session_dirty();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        closed.len()
    }

    /// Close the region whose program exited, if the pane belongs to one.
    ///
    /// Returns the closed region's id, so the caller can report the exit. A
    /// non-zero exit raises a toast naming the plugin and status; there is no
    /// restart loop, because a region that fails on startup would otherwise
    /// respawn forever.
    pub(crate) fn close_region_for_exited_pane(&mut self, pane_id: PaneId) -> Option<String> {
        let region = self.state.regions.find_by_pane(pane_id)?;
        let region_id = region.region_id.to_string();
        let plugin_id = region.plugin_id.clone();
        let title = region.title.clone();
        let exit_code = self
            .terminal_runtimes
            .get(&region.terminal_id)
            .and_then(crate::terminal::TerminalRuntime::child_exit_code);

        if !self.close_region(&region_id) {
            return None;
        }

        if let Some(code) = exit_code.filter(|code| *code != 0) {
            self.state.toast = Some(crate::app::state::ToastNotification {
                kind: crate::app::state::ToastKind::NeedsAttention,
                title: format!("{title} closed"),
                context: format!("{plugin_id} exited with status {code}"),
                position: None,
                // No target: a region is not a pane, so there is nothing to
                // focus when the toast is clicked.
                target: None,
            });
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }

        Some(region_id)
    }

    pub(crate) fn focus_region(&mut self, region_id: &str) {
        self.state.region_focus = Some(region_id.to_string());
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Drop a region's terminal state and runtime, and clear stale focus.
    fn release_region_runtime(&mut self, region: &RegionInstance) {
        if self.state.region_focus.as_deref() == Some(region.region_id.as_str()) {
            self.state.region_focus = None;
        }
        self.state
            .direct_attach_resize_locks
            .remove(&region.terminal_id);
        self.state.terminals.remove(&region.terminal_id);
        self.shutdown_terminal_runtime(region.terminal_id.clone());
    }
}
