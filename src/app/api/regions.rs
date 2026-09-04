//! `region.*` API handlers.
//!
//! A region is host-owned screen area outside any tab's pane tree. These
//! handlers own the slot: they validate the request against the owning plugin's
//! manifest, clamp every size server-side, and keep the one-region-per-slot
//! invariant. Spawning the terminal and tearing it down lives in
//! `crate::app::region`, mirroring how `crate::app::popup` serves the popup.

use super::plugins::{
    normalize_plugin_id, plugin_effective_platforms, plugin_ensure_platform_supported,
    plugin_normalize_action_id,
};
use super::responses::{encode_error, encode_success};
use crate::api::schema::{
    RegionInfo, RegionOpenParams, RegionResizeParams, RegionTarget, ResponseResult,
};
use crate::app::App;
use crate::region::{RegionInstance, RegionSizeBounds};

impl App {
    pub(super) fn handle_region_open(&mut self, id: String, params: RegionOpenParams) -> String {
        if let Err(err) = self.refresh_installed_plugins() {
            return encode_error(id, "plugin_registry_load_failed", err.to_string());
        }
        let Some(plugin_id) = normalize_plugin_id(&params.plugin_id) else {
            return encode_error(id, "invalid_plugin_id", "invalid plugin id");
        };
        let Some(plugin) = self.state.installed_plugins.get(&plugin_id).cloned() else {
            return encode_error(id, "plugin_not_found", "plugin not found");
        };
        if !plugin.enabled {
            return encode_error(
                id,
                "plugin_disabled",
                format!("plugin {plugin_id} is disabled"),
            );
        }
        let Some(entrypoint) = plugin_normalize_action_id(&params.entrypoint) else {
            return encode_error(id, "invalid_plugin_entrypoint", "invalid entrypoint id");
        };
        // A plugin may only open regions declared in its own manifest.
        let Some(manifest) = plugin
            .regions
            .iter()
            .find(|region| region.id == entrypoint)
            .cloned()
        else {
            return encode_error(
                id,
                "plugin_region_not_found",
                format!("plugin region entrypoint '{entrypoint}' not found"),
            );
        };
        if let Err((code, message)) = plugin_ensure_platform_supported(
            plugin_effective_platforms(&manifest.platforms, &plugin.platforms),
            "plugin region",
        ) {
            return encode_error(id, code, message);
        }

        let config = self.state.regions_config.right;
        if !config.enabled {
            return encode_error(
                id,
                "region_anchor_disabled",
                "the right region is disabled by configuration",
            );
        }
        // The slot invariant: a second open is an error, never a silent replace.
        if self.state.regions.get(manifest.anchor).is_some() {
            return encode_error(
                id,
                "region_slot_occupied",
                "a region is already open at this anchor and scope",
            );
        }

        // Bounds come from the manifest and are clamped into absolute limits by
        // RegionSizeBounds, so neither the manifest nor this caller can widen
        // them, zero the region, or make it consume the frame.
        let bounds = RegionSizeBounds::new(manifest.min_size, manifest.max_size);
        // An explicit request wins, then user config, then the manifest's
        // preferred size, then the built-in default.
        let requested = params
            .size
            .or(config.size)
            .or(manifest.size)
            .unwrap_or(crate::region::DEFAULT_REGION_SIZE);

        match self.open_region(&plugin, &manifest, bounds, requested, params) {
            Ok(region_id) => {
                let Some(region) = self.state.regions.get_by_id(region_id.as_str()) else {
                    return encode_error(id, "region_open_failed", "region disappeared after open");
                };
                let info = region_info(region, self.state.region_focus.as_deref());
                encode_success(id, ResponseResult::RegionOpened { region: info })
            }
            Err(err) => encode_error(id, "region_open_failed", err.to_string()),
        }
    }

    pub(super) fn handle_region_close(&mut self, id: String, params: RegionTarget) -> String {
        if self.close_region(&params.region_id) {
            encode_success(
                id,
                ResponseResult::RegionClosed {
                    region_id: params.region_id,
                },
            )
        } else {
            encode_error(id, "region_not_found", "no region with that id is open")
        }
    }

    pub(super) fn handle_region_focus(&mut self, id: String, params: RegionTarget) -> String {
        let Some(region) = self.state.regions.get_by_id(&params.region_id) else {
            return encode_error(id, "region_not_found", "no region with that id is open");
        };
        if !region.visible {
            return encode_error(
                id,
                "region_hidden",
                "a hidden region cannot take keyboard focus",
            );
        }
        self.focus_region(&params.region_id);
        let Some(region) = self.state.regions.get_by_id(&params.region_id) else {
            return encode_error(id, "region_not_found", "no region with that id is open");
        };
        let info = region_info(region, self.state.region_focus.as_deref());
        encode_success(id, ResponseResult::RegionFocused { region: info })
    }

    pub(super) fn handle_region_resize(
        &mut self,
        id: String,
        params: RegionResizeParams,
    ) -> String {
        let focus = self.state.region_focus.clone();
        let Some(region) = self.state.regions.get_by_id_mut(&params.region_id) else {
            return encode_error(id, "region_not_found", "no region with that id is open");
        };
        // Clamped into the region's server-owned bounds; an out-of-range value
        // is applied as the nearest allowed size, never accepted as given.
        region.set_size(params.size);
        let info = region_info(region, focus.as_deref());
        self.state.mark_session_dirty();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        encode_success(id, ResponseResult::RegionResized { region: info })
    }

    pub(super) fn handle_region_list(&mut self, id: String) -> String {
        let focus = self.state.region_focus.as_deref();
        let regions = self
            .state
            .regions
            .iter()
            .map(|region| region_info(region, focus))
            .collect();
        encode_success(id, ResponseResult::RegionList { regions })
    }
}

fn region_info(region: &RegionInstance, focus: Option<&str>) -> RegionInfo {
    let bounds = region.bounds();
    RegionInfo {
        region_id: region.region_id.to_string(),
        plugin_id: region.plugin_id.clone(),
        entrypoint: region.entrypoint.clone(),
        title: region.title.clone(),
        anchor: region.anchor,
        scope: region.scope,
        size: region.size(),
        min_size: bounds.min(),
        max_size: bounds.max(),
        visible: region.visible,
        focused: focus == Some(region.region_id.as_str()),
    }
}
