//! JSON API handlers for desktop regions.

use super::super::responses::{encode_error, encode_success};
use super::manifest::{effective_platforms, ensure_platform_supported};
use super::manifest::{normalize_action_id, normalize_plugin_id};
use super::{invalid_plugin_id, plugin_manifest_available};
use crate::api::schema::{
    RegionCloseReason, RegionOpenParams, RegionResizeParams, RegionTarget, ResponseResult,
};
use crate::app::region::RegionLaunch;
use crate::app::App;
use crate::region::RegionSizeBounds;

impl App {
    pub(in crate::app::api) fn handle_region_open(
        &mut self,
        id: String,
        params: RegionOpenParams,
    ) -> String {
        if let Err(err) = self.refresh_installed_plugins() {
            return encode_error(id, "plugin_registry_load_failed", err.to_string());
        }
        let Some(plugin_id) = normalize_plugin_id(&params.plugin_id) else {
            return invalid_plugin_id(id);
        };
        let launch = match self.resolve_region_launch(
            &plugin_id,
            &params.entrypoint,
            params.env,
            params.size,
            params.focus,
        ) {
            Ok(launch) => launch,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        match self.open_region(launch) {
            Ok(region) => encode_success(id, ResponseResult::RegionInfo { region }),
            Err(err) => encode_error(id, err.code(), err.message()),
        }
    }

    /// Build a region launch from the plugin's own manifest. Refuses plugins that are
    /// missing, unavailable or disabled, and entrypoints the manifest does not declare.
    pub(crate) fn resolve_region_launch(
        &self,
        plugin_id: &str,
        entrypoint: &str,
        env: std::collections::HashMap<String, String>,
        requested_size: Option<u32>,
        focus: bool,
    ) -> Result<RegionLaunch, (String, String)> {
        let Some(plugin) = self.state.installed_plugins.get(plugin_id).cloned() else {
            return Err(("plugin_not_found".into(), "plugin not found".into()));
        };
        if !plugin_manifest_available(&plugin) {
            return Err((
                "plugin_manifest_unavailable".into(),
                format!("plugin {plugin_id} manifest is unavailable"),
            ));
        }
        if !plugin.enabled {
            return Err((
                "plugin_disabled".into(),
                format!("plugin {plugin_id} is disabled"),
            ));
        }
        let Some(entrypoint) = normalize_action_id(entrypoint) else {
            return Err((
                "invalid_plugin_entrypoint".into(),
                "invalid entrypoint id".into(),
            ));
        };
        let Some(region) = plugin
            .regions
            .iter()
            .find(|region| region.id == entrypoint)
            .cloned()
        else {
            return Err((
                "plugin_region_not_found".into(),
                format!("plugin region entrypoint '{entrypoint}' not found"),
            ));
        };
        ensure_platform_supported(
            effective_platforms(&region.platforms, &plugin.platforms),
            "plugin region",
        )
        .map_err(|(code, message)| (code.to_string(), message))?;
        let Some(program) = region.command.first() else {
            return Err((
                "invalid_plugin_manifest".into(),
                "region command is empty".into(),
            ));
        };
        let mut argv = region.command.clone();
        argv[0] = crate::plugin_command::program_for_cwd(
            program,
            std::path::Path::new(&plugin.plugin_root),
        )
        .display()
        .to_string();
        let context = self.current_plugin_context("plugin-region");
        let cwd = self.plugin_pane_cwd(&plugin, None);
        let env = self.plugin_pane_launch_env(&plugin, &region.id, &cwd, env, &context)?;
        Ok(RegionLaunch {
            plugin_id: plugin.plugin_id.clone(),
            plugin_name: plugin.name.clone(),
            entrypoint: region.id.clone(),
            title: region.title.clone(),
            edge: region.edge,
            scope: region.scope,
            argv,
            cwd,
            env,
            manifest_size: region.size,
            bounds: RegionSizeBounds::new(region.min_size, region.max_size),
            requested_size,
            focus,
        })
    }

    pub(in crate::app::api) fn handle_region_close(
        &mut self,
        id: String,
        params: RegionTarget,
    ) -> String {
        match self.close_region(&params.region_id, RegionCloseReason::Requested, None) {
            Some(region) => encode_success(
                id,
                ResponseResult::RegionClosed {
                    region_id: region.region_id,
                    reason: RegionCloseReason::Requested,
                },
            ),
            None => region_not_found(id, &params.region_id),
        }
    }

    pub(in crate::app::api) fn handle_region_focus(
        &mut self,
        id: String,
        params: RegionTarget,
    ) -> String {
        match self.focus_region(&params.region_id) {
            Ok(region) => encode_success(id, ResponseResult::RegionInfo { region }),
            Err(err) => encode_error(id, err.code(), err.message(&params.region_id)),
        }
    }

    pub(in crate::app::api) fn handle_region_resize(
        &mut self,
        id: String,
        params: RegionResizeParams,
    ) -> String {
        match self.resize_region(&params.region_id, params.size) {
            Ok(region) => encode_success(id, ResponseResult::RegionInfo { region }),
            Err(err) => encode_error(id, err.code(), err.message(&params.region_id)),
        }
    }

    pub(in crate::app::api) fn handle_region_set_visible(
        &mut self,
        id: String,
        params: RegionTarget,
        visible: bool,
    ) -> String {
        match self.set_region_visible(&params.region_id, visible) {
            Ok(region) => encode_success(id, ResponseResult::RegionInfo { region }),
            Err(err) => encode_error(id, err.code(), err.message(&params.region_id)),
        }
    }

    pub(in crate::app::api) fn handle_region_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::RegionList {
                regions: self.state.region_infos(),
            },
        )
    }
}

fn region_not_found(id: String, region_id: &str) -> String {
    encode_error(
        id,
        "region_not_found",
        format!("region {region_id} not found"),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use crate::api::schema::{
        EmptyParams, EventData, EventKind, LayoutExportParams, Method, PaneLayoutParams,
        PaneListParams, PaneTarget, PluginLinkParams, PluginSetEnabledParams, PluginUnlinkParams,
        RegionCloseReason, RegionOpenParams, RegionResizeParams, RegionTarget, Request,
        ResponseResult, SuccessResponse,
    };
    use crate::app::App;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const BOARD_PLUGIN: &str = "example.region-board";

    fn test_app_with_hub(hub: crate::api::EventHub) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            hub,
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("regions")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = crate::app::Mode::Terminal;
        app
    }

    fn test_app() -> App {
        test_app_with_hub(crate::api::EventHub::default())
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    fn request(app: &mut App, method: Method) -> String {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        app.state.assert_invariants_for_test();
        response
    }

    fn success(response: &str) -> ResponseResult {
        serde_json::from_str::<SuccessResponse>(response)
            .unwrap_or_else(|err| panic!("expected success, got {response}: {err}"))
            .result
    }

    fn error_code(response: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(response).expect("json response");
        value["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("expected error, got {response}"))
            .to_string()
    }

    fn link_plugin(app: &mut App, name: &str, manifest: &str) -> PathBuf {
        let root = unique_temp_path(name);
        std::fs::create_dir_all(&root).expect("plugin root");
        std::fs::write(root.join("herdr-plugin.toml"), manifest).expect("manifest");
        let linked = request(
            app,
            Method::PluginLink(PluginLinkParams {
                path: root.display().to_string(),
                enabled: true,
                source: None,
            }),
        );
        assert!(linked.contains("plugin_linked"), "link failed: {linked}");
        root
    }

    fn board_manifest(command: &str) -> String {
        format!(
            r#"
id = "{BOARD_PLUGIN}"
name = "Region Board"
version = "0.1.0"
min_herdr_version = "0.6.10"
platforms = ["linux", "macos"]

[[panes]]
id = "pane-only"
title = "Pane Only"
command = ["sh", "-c", "sleep 30"]

[[regions]]
id = "board"
title = "Board"
size = 40
min_size = 12
max_size = 60
command = ["sh", "-c", {command:?}]
"#
        )
    }

    fn link_board(app: &mut App) -> PathBuf {
        link_plugin(app, "region-board", &board_manifest("sleep 30"))
    }

    fn open_params(plugin_id: &str, entrypoint: &str) -> RegionOpenParams {
        RegionOpenParams {
            plugin_id: plugin_id.into(),
            entrypoint: entrypoint.into(),
            size: None,
            focus: false,
            env: HashMap::new(),
        }
    }

    fn open_board(app: &mut App, params: RegionOpenParams) -> crate::api::schema::RegionInfo {
        let response = request(app, Method::RegionOpen(params));
        let ResponseResult::RegionInfo { region } = success(&response) else {
            panic!("expected region info: {response}");
        };
        region
    }

    fn target(region_id: &str) -> RegionTarget {
        RegionTarget {
            region_id: region_id.into(),
        }
    }

    fn cleanup(mut app: App, roots: &[PathBuf]) {
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        for root in roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn region_pid(app: &App, region_id: &str) -> u32 {
        let region = app.state.regions.get_by_id(region_id).expect("region");
        let runtime = app
            .terminal_runtimes
            .get(&region.terminal_id)
            .expect("region runtime");
        let mut pid = None;
        wait_until(
            || {
                pid = runtime.child_pid();
                pid.is_some()
            },
            "region child pid",
        );
        pid.expect("pid")
    }

    // Invariant 1: a second open in an occupied slot fails and leaves the occupant alone.
    #[tokio::test]
    async fn second_open_in_occupied_slot_is_refused_without_replacing() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let first = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        let first_terminal = app
            .state
            .regions
            .get_by_id(&first.region_id)
            .expect("first region")
            .terminal_id
            .clone();

        let second = request(
            &mut app,
            Method::RegionOpen(open_params(BOARD_PLUGIN, "board")),
        );

        assert_eq!(error_code(&second), "region_slot_occupied");
        let regions = app.state.region_infos();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].region_id, first.region_id);
        assert!(app.terminal_runtimes.get(&first_terminal).is_some());
        cleanup(app, &[root]);
    }

    // Invariant 3: only regions declared in the caller's own manifest, while enabled.
    #[tokio::test]
    async fn open_refuses_undeclared_foreign_and_disabled_entrypoints() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let other_root = link_plugin(
            &mut app,
            "region-other",
            r#"
id = "example.region-other"
name = "Other"
version = "0.1.0"
min_herdr_version = "0.6.10"
platforms = ["linux", "macos"]

[[regions]]
id = "meter"
title = "Meter"
command = ["sh", "-c", "sleep 30"]
"#,
        );

        // A pane entrypoint is not a region.
        let pane_only = request(
            &mut app,
            Method::RegionOpen(open_params(BOARD_PLUGIN, "pane-only")),
        );
        assert_eq!(error_code(&pane_only), "plugin_region_not_found");
        // Another plugin's region cannot be opened under this plugin's id.
        let foreign = request(
            &mut app,
            Method::RegionOpen(open_params(BOARD_PLUGIN, "meter")),
        );
        assert_eq!(error_code(&foreign), "plugin_region_not_found");
        let missing = request(
            &mut app,
            Method::RegionOpen(open_params("example.not-installed", "board")),
        );
        assert_eq!(error_code(&missing), "plugin_not_found");

        request(
            &mut app,
            Method::PluginDisable(PluginSetEnabledParams {
                plugin_id: BOARD_PLUGIN.into(),
            }),
        );
        let disabled = request(
            &mut app,
            Method::RegionOpen(open_params(BOARD_PLUGIN, "board")),
        );
        assert_eq!(error_code(&disabled), "plugin_disabled");
        assert!(app.state.regions.is_empty());
        cleanup(app, &[root, other_root]);
    }

    // Invariant 9: caller env cannot overwrite Herdr-owned variables.
    #[tokio::test]
    async fn hostile_env_cannot_overwrite_herdr_owned_variables() {
        let mut app = test_app();
        let capture = unique_temp_path("region-env-capture");
        let script = format!(
            "printf '%s\\n%s\\n%s\\n%s\\n%s\\n' \"$HERDR_PLUGIN_ID\" \"$HERDR_ENV\" \"$HERDR_PLUGIN_ENTRYPOINT_ID\" \"$HERDR_SOCKET_PATH\" \"$REGION_USER_VAR\" > {}; sleep 30",
            capture.display()
        );
        let root = link_plugin(&mut app, "region-env", &board_manifest(&script));
        let mut params = open_params(BOARD_PLUGIN, "board");
        params.env = HashMap::from([
            ("HERDR_PLUGIN_ID".to_string(), "spoofed.plugin".to_string()),
            ("HERDR_ENV".to_string(), "0".to_string()),
            (
                "HERDR_PLUGIN_ENTRYPOINT_ID".to_string(),
                "spoofed".to_string(),
            ),
            (
                "HERDR_SOCKET_PATH".to_string(),
                "/tmp/spoofed.sock".to_string(),
            ),
            ("REGION_USER_VAR".to_string(), "kept".to_string()),
        ]);
        open_board(&mut app, params);

        let mut text = String::new();
        wait_until(
            || {
                text = std::fs::read_to_string(&capture).unwrap_or_default();
                text.lines().count() >= 5
            },
            "region env capture",
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], BOARD_PLUGIN);
        assert_eq!(lines[1], "1");
        assert_eq!(lines[2], "board");
        assert_ne!(lines[3], "/tmp/spoofed.sock");
        assert_eq!(lines[3], crate::api::socket_path().display().to_string());
        assert_eq!(lines[4], "kept");
        let _ = std::fs::remove_file(&capture);
        cleanup(app, &[root]);
    }

    // Invariant 9: the launch env holds every Herdr-owned key exactly once, with Herdr's
    // value, so no spawn path can pick the caller's copy by ordering.
    #[tokio::test]
    async fn launch_env_never_carries_caller_copies_of_herdr_owned_keys() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let plugin = app
            .state
            .installed_plugins
            .get(BOARD_PLUGIN)
            .cloned()
            .expect("linked plugin");
        let protected = [
            "HERDR_SOCKET_PATH",
            "HERDR_ENV",
            "HERDR_PLUGIN_ID",
            "HERDR_PLUGIN_ROOT",
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
            "HERDR_PLUGIN_ENTRYPOINT_ID",
            "HERDR_PLUGIN_CONTEXT_JSON",
            "HERDR_BIN_PATH",
        ];
        let hostile: HashMap<String, String> = protected
            .iter()
            .map(|key| ((*key).to_string(), "spoofed".to_string()))
            .collect();
        let context = app.current_plugin_context("plugin-region");
        let cwd = std::path::PathBuf::from(&plugin.plugin_root);
        let env = app
            .plugin_pane_launch_env(&plugin, "board", &cwd, hostile, &context)
            .expect("launch env");

        for key in protected {
            let values: Vec<&str> = env
                .iter()
                .filter(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
                .collect();
            assert!(values.len() <= 1, "{key} appears {} times", values.len());
            assert!(!values.contains(&"spoofed"), "{key} kept the caller value");
        }
        cleanup(app, &[root]);
    }

    // Invariant 4: every size the server applies is clamped to the region bounds.
    #[tokio::test]
    async fn open_and_resize_clamp_hostile_sizes_to_manifest_bounds() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let mut params = open_params(BOARD_PLUGIN, "board");
        params.size = Some(0);
        let opened = open_board(&mut app, params);
        assert_eq!(opened.size, 12, "zero clamps up to the manifest minimum");
        assert_eq!((opened.min_size, opened.max_size), (12, 60));

        let resize = |app: &mut App, size: u32| {
            let response = request(
                app,
                Method::RegionResize(RegionResizeParams {
                    region_id: opened.region_id.clone(),
                    size,
                }),
            );
            let ResponseResult::RegionInfo { region } = success(&response) else {
                panic!("expected region info: {response}");
            };
            region.size
        };
        assert_eq!(resize(&mut app, u32::MAX), 60);
        assert_eq!(resize(&mut app, 0), 12);
        assert_eq!(resize(&mut app, 70_000), 60, "values past u16 saturate");
        assert_eq!(resize(&mut app, 33), 33);

        let unknown = request(
            &mut app,
            Method::RegionResize(RegionResizeParams {
                region_id: "region_missing".into(),
                size: 20,
            }),
        );
        assert_eq!(error_code(&unknown), "region_not_found");
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn open_size_precedence_is_request_then_config_then_manifest() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let manifest_default = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        assert_eq!(manifest_default.size, 40);
        request(
            &mut app,
            Method::RegionClose(target(&manifest_default.region_id)),
        );

        app.state.regions_config.right.size = Some(50);
        let configured = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        assert_eq!(configured.size, 50);
        request(&mut app, Method::RegionClose(target(&configured.region_id)));

        app.state.regions_config.right.size = Some(500);
        let config_clamped = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        assert_eq!(config_clamped.size, 60, "config is still clamped");
        request(
            &mut app,
            Method::RegionClose(target(&config_clamped.region_id)),
        );

        let mut params = open_params(BOARD_PLUGIN, "board");
        params.size = Some(20);
        let requested = open_board(&mut app, params);
        assert_eq!(requested.size, 20);
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn disabled_edge_refuses_open() {
        let mut app = test_app();
        let root = link_board(&mut app);
        app.state.regions_config.right.enabled = false;
        let response = request(
            &mut app,
            Method::RegionOpen(open_params(BOARD_PLUGIN, "board")),
        );
        assert_eq!(error_code(&response), "region_edge_disabled");
        assert!(app.state.regions.is_empty());
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn close_kills_the_process_and_a_second_close_reports_not_found() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let region = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        let pid = region_pid(&app, &region.region_id);
        let terminal_count = app.terminal_runtimes.len();

        let closed = request(&mut app, Method::RegionClose(target(&region.region_id)));
        let ResponseResult::RegionClosed { region_id, reason } = success(&closed) else {
            panic!("expected region closed: {closed}");
        };
        assert_eq!(region_id, region.region_id);
        assert_eq!(reason, RegionCloseReason::Requested);
        assert_eq!(app.terminal_runtimes.len(), terminal_count - 1);
        wait_until(|| !process_alive(pid), "region process to exit");

        let again = request(&mut app, Method::RegionClose(target(&region.region_id)));
        assert_eq!(error_code(&again), "region_not_found");
        cleanup(app, &[root]);
    }

    // Invariant 5: disable and unlink close the region and kill its process.
    #[tokio::test]
    async fn disabling_the_plugin_closes_its_region_and_kills_the_process() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let root = link_board(&mut app);
        let region = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        let pid = region_pid(&app, &region.region_id);

        request(
            &mut app,
            Method::PluginDisable(PluginSetEnabledParams {
                plugin_id: BOARD_PLUGIN.into(),
            }),
        );

        assert!(app.state.regions.is_empty());
        wait_until(|| !process_alive(pid), "region process to exit");
        let reasons: Vec<RegionCloseReason> = hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::RegionClosed { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, vec![RegionCloseReason::PluginDisabled]);
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn unlinking_the_plugin_closes_its_region_and_kills_the_process() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let root = link_board(&mut app);
        let region = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        let pid = region_pid(&app, &region.region_id);

        request(
            &mut app,
            Method::PluginUnlink(PluginUnlinkParams {
                plugin_id: BOARD_PLUGIN.into(),
            }),
        );

        assert!(app.state.regions.is_empty());
        wait_until(|| !process_alive(pid), "region process to exit");
        let reasons: Vec<RegionCloseReason> = hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::RegionClosed { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, vec![RegionCloseReason::PluginRemoved]);
        cleanup(app, &[root]);
    }

    // Invariant 2: a region is not a pane and never reports as the focused pane.
    #[tokio::test]
    async fn region_is_absent_from_pane_list_and_its_focus_unfocuses_every_pane() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let pane_count_before = match success(&request(
            &mut app,
            Method::PaneList(PaneListParams { workspace_id: None }),
        )) {
            ResponseResult::PaneList { panes } => panes.len(),
            other => panic!("expected pane list: {other:?}"),
        };
        let mut params = open_params(BOARD_PLUGIN, "board");
        params.focus = true;
        let region = open_board(&mut app, params);
        assert!(region.focused);
        let region_terminal = app
            .state
            .regions
            .get_by_id(&region.region_id)
            .expect("region")
            .terminal_id
            .to_string();

        let ResponseResult::PaneList { panes } = success(&request(
            &mut app,
            Method::PaneList(PaneListParams { workspace_id: None }),
        )) else {
            panic!("expected pane list");
        };
        assert_eq!(panes.len(), pane_count_before);
        assert!(panes.iter().all(|pane| pane.terminal_id != region_terminal));
        assert!(
            panes.iter().all(|pane| !pane.focused),
            "no pane is focused while a region holds focus"
        );

        // Every other listing of panes, layouts and agents must not mention the region.
        for method in [
            Method::SessionSnapshot(EmptyParams::default()),
            Method::AgentList(EmptyParams::default()),
            Method::PaneLayout(PaneLayoutParams::default()),
            Method::LayoutExport(LayoutExportParams {
                tab_id: None,
                pane_id: None,
            }),
        ] {
            let response = request(&mut app, method);
            success(&response);
            assert!(
                !response.contains(&region_terminal) && !response.contains(&region.region_id),
                "region leaked into {response}"
            );
        }
        cleanup(app, &[root]);
    }

    // Invariant 7: pane focus clears region focus; region focus needs a visible region.
    #[tokio::test]
    async fn pane_focus_clears_region_focus_and_hidden_regions_refuse_focus() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let root = link_board(&mut app);
        let region = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        let focused = request(&mut app, Method::RegionFocus(target(&region.region_id)));
        let ResponseResult::RegionInfo { region: info } = success(&focused) else {
            panic!("expected region info: {focused}");
        };
        assert!(info.focused);

        let ResponseResult::PaneList { panes } = success(&request(
            &mut app,
            Method::PaneList(PaneListParams { workspace_id: None }),
        )) else {
            panic!("expected pane list");
        };
        // Re-focusing the pane that was already focused still clears region focus.
        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .and_then(|pane| app.public_pane_id(0, pane))
            .expect("focused pane");
        assert!(panes.iter().any(|pane| pane.pane_id == pane_id));
        request(
            &mut app,
            Method::PaneFocus(PaneTarget {
                pane_id: pane_id.clone(),
            }),
        );
        assert!(app.state.region_focus.is_none());
        assert!(app.state.focused_region_terminal().is_none());
        assert!(!app.state.region_infos()[0].focused);

        request(&mut app, Method::RegionFocus(target(&region.region_id)));
        request(&mut app, Method::RegionHide(target(&region.region_id)));
        assert!(app.state.region_focus.is_none(), "hide releases focus");
        let hidden_focus = request(&mut app, Method::RegionFocus(target(&region.region_id)));
        assert_eq!(error_code(&hidden_focus), "region_hidden");

        let shown = request(&mut app, Method::RegionShow(target(&region.region_id)));
        let ResponseResult::RegionInfo { region: shown } = success(&shown) else {
            panic!("expected region info: {shown}");
        };
        assert!(shown.visible && !shown.focused);
        let kinds: Vec<EventKind> = hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::RegionOpened
                        | EventKind::RegionClosed
                        | EventKind::RegionFocused
                        | EventKind::RegionResized
                        | EventKind::RegionVisibilityChanged
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::RegionOpened,
                EventKind::RegionFocused,
                EventKind::RegionFocused,
                EventKind::RegionVisibilityChanged,
                EventKind::RegionVisibilityChanged,
            ]
        );
        cleanup(app, &[root]);
    }

    // Invariant 7: a pane focus change made outside the API still releases region focus.
    #[tokio::test]
    async fn pane_focus_change_outside_the_api_makes_region_focus_inert() {
        let mut app = test_app();
        let runtime = crate::terminal::TerminalRuntime::test_with_channel(10, 5).0;
        let region_id = app.install_test_region("example.inert", runtime);
        app.focus_region(region_id.as_str()).expect("focus");
        assert!(app.state.focused_region_terminal().is_some());

        let other = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        app.state.focus_pane_in_workspace(0, other);

        assert!(app.state.focused_region_terminal().is_none());
        assert!(app.state.reconcile_region_focus());
        assert!(app.state.region_focus.is_none());
        app.state.assert_invariants_for_test();
        cleanup(app, &[]);
    }

    // Invariant 6: exit closes the region; only a non-zero exit raises a toast.
    #[tokio::test]
    async fn process_exit_closes_the_region_and_toasts_only_on_failure() {
        for (status, expect_toast) in [
            (
                crate::pane::ChildExitStatus {
                    code: 0,
                    signal: None,
                },
                false,
            ),
            (
                crate::pane::ChildExitStatus {
                    code: 3,
                    signal: None,
                },
                true,
            ),
        ] {
            let hub = crate::api::EventHub::default();
            let mut app = test_app_with_hub(hub.clone());
            app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;
            let runtime = crate::terminal::TerminalRuntime::test_with_channel(10, 5).0;
            runtime.record_test_child_exit(status.clone());
            let region_id = app.install_test_region("example.exit", runtime);
            let pane_id = app
                .state
                .regions
                .get_by_id(region_id.as_str())
                .expect("region")
                .pane_id;

            app.handle_internal_event(crate::events::AppEvent::PaneDied {
                pane_id,
                exit_reason: crate::platform::ChildExitReason::Exited,
            });

            assert!(app.state.regions.is_empty());
            app.state.assert_invariants_for_test();
            let closed: Vec<_> = hub
                .events_after(0)
                .into_iter()
                .filter_map(|(_, event)| match event.data {
                    EventData::RegionClosed { reason, exit, .. } => Some((reason, exit)),
                    _ => None,
                })
                .collect();
            assert_eq!(closed.len(), 1);
            assert_eq!(closed[0].0, RegionCloseReason::Exited);
            assert_eq!(
                closed[0].1.as_ref().map(|exit| exit.code),
                Some(status.code)
            );
            match (&app.state.toast, expect_toast) {
                (Some(toast), true) => {
                    assert!(toast.title.contains("example.exit"), "{}", toast.title);
                    assert!(toast.context.contains('3'), "{}", toast.context);
                }
                (None, false) => {}
                (toast, _) => panic!("unexpected toast state {toast:?} for {status:?}"),
            }
            cleanup(app, &[]);
        }
    }

    #[tokio::test]
    async fn region_list_reports_open_regions() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let ResponseResult::RegionList { regions } = success(&request(
            &mut app,
            Method::RegionList(crate::api::schema::EmptyParams {}),
        )) else {
            panic!("expected region list");
        };
        assert!(regions.is_empty());
        let region = open_board(&mut app, open_params(BOARD_PLUGIN, "board"));
        let ResponseResult::RegionList { regions } = success(&request(
            &mut app,
            Method::RegionList(crate::api::schema::EmptyParams {}),
        )) else {
            panic!("expected region list");
        };
        assert_eq!(regions, vec![region]);
        cleanup(app, &[root]);
    }

    fn region_snapshot(
        plugin_id: &str,
        entrypoint: &str,
        size: u16,
        visible: bool,
    ) -> crate::persist::RegionSnapshot {
        crate::persist::RegionSnapshot {
            plugin_id: plugin_id.into(),
            entrypoint: entrypoint.into(),
            edge: crate::region::RegionEdge::Right,
            scope: crate::region::RegionScope::Session,
            size,
            visible,
        }
    }

    #[tokio::test]
    async fn restore_respawns_regions_with_clamped_size_and_visibility_never_focused() {
        let mut app = test_app();
        let root = link_board(&mut app);

        // The board manifest bounds sizes to 12..=60; 500 is a hostile hand-edited value.
        app.restore_regions(&[region_snapshot(BOARD_PLUGIN, "board", 500, false)]);
        app.state.assert_invariants_for_test();

        let regions = app.state.region_infos();
        assert_eq!(regions.len(), 1, "{regions:?}");
        assert_eq!(regions[0].plugin_id, BOARD_PLUGIN);
        assert_eq!(regions[0].entrypoint, "board");
        assert_eq!(regions[0].size, 60);
        assert!(!regions[0].visible);
        assert!(!regions[0].focused);
        assert!(app.state.region_focus.is_none());
        let terminal = &app
            .state
            .regions
            .get_by_id(&regions[0].region_id)
            .expect("restored region")
            .terminal_id;
        assert!(app.terminal_runtimes.get(terminal).is_some());
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn restore_drops_regions_whose_plugin_is_gone_disabled_or_undeclared() {
        let mut app = test_app();
        let root = link_board(&mut app);
        request(
            &mut app,
            Method::PluginDisable(PluginSetEnabledParams {
                plugin_id: BOARD_PLUGIN.into(),
            }),
        );
        let runtimes_before = app.terminal_runtimes.len();

        app.restore_regions(&[
            region_snapshot(BOARD_PLUGIN, "board", 30, true),
            region_snapshot("example.missing", "board", 30, true),
        ]);
        assert!(app.state.region_infos().is_empty());

        request(
            &mut app,
            Method::PluginEnable(PluginSetEnabledParams {
                plugin_id: BOARD_PLUGIN.into(),
            }),
        );
        app.restore_regions(&[region_snapshot(BOARD_PLUGIN, "pane-only", 30, true)]);

        assert!(app.state.region_infos().is_empty());
        assert_eq!(app.terminal_runtimes.len(), runtimes_before);
        app.state.assert_invariants_for_test();
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn restore_keeps_only_the_first_region_for_a_slot() {
        let mut app = test_app();
        let root = link_board(&mut app);

        app.restore_regions(&[
            region_snapshot(BOARD_PLUGIN, "board", 20, true),
            region_snapshot(BOARD_PLUGIN, "board", 50, true),
        ]);

        let regions = app.state.region_infos();
        assert_eq!(regions.len(), 1, "{regions:?}");
        assert_eq!(regions[0].size, 20);
        app.state.assert_invariants_for_test();
        cleanup(app, &[root]);
    }

    #[tokio::test]
    async fn session_save_captures_open_regions() {
        let mut app = test_app();
        let root = link_board(&mut app);
        let mut params = open_params(BOARD_PLUGIN, "board");
        params.size = Some(25);
        let region = open_board(&mut app, params);
        request(&mut app, Method::RegionHide(target(&region.region_id)));

        let snapshot = crate::persist::capture_regions(&app.state.regions);

        assert_eq!(
            snapshot,
            vec![region_snapshot(BOARD_PLUGIN, "board", 25, false)]
        );
        cleanup(app, &[root]);
    }
}
