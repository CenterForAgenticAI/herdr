//! JSON API shapes for desktop regions.
//!
//! A region is a strip of the client frame outside every tab's pane tree. It is
//! addressed by `region_id` only and never appears in pane APIs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::cli::projection::{cli_params, Field};
use crate::cli::region::bindings;
use crate::region::{RegionEdge, RegionScope};

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct RegionOpenParams => RegionOpenProjection {
        pub plugin_id: String => Field::new(String::new()).input(bindings::positional_text("plugin_id", 0, "PLUGIN_ID")),
        pub entrypoint: String => Field::new(String::new()).input(bindings::positional_text("entrypoint", 1, "ENTRYPOINT")),
        /// Requested width in cells. The server clamps it to the manifest bounds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub size: Option<u32> => Field::new(None).input(bindings::optional_size()),
        #[serde(default, skip_serializing_if = "super::is_false")]
        pub focus: bool => Field::new(false).input(bindings::boolean("focus", true)),
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        pub env: HashMap<String, String> => Field::new(HashMap::new()).input(bindings::environment()),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct RegionTarget => RegionTargetProjection {
        pub region_id: String => Field::new(String::new()).input(bindings::region_id()),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct RegionResizeParams => RegionResizeProjection {
        pub region_id: String => Field::new(String::new()).input(bindings::region_id()),
        /// Requested width in cells. The server clamps it to the region's bounds.
        pub size: u32 => Field::new(0).input(bindings::required_size()),
    }
}

/// Public description of one live region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionInfo {
    pub region_id: String,
    pub plugin_id: String,
    pub entrypoint: String,
    pub title: String,
    pub edge: RegionEdge,
    pub scope: RegionScope,
    /// Applied width in cells, after clamping.
    pub size: u16,
    pub min_size: u16,
    pub max_size: u16,
    pub visible: bool,
    pub focused: bool,
}

/// Why a region closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegionCloseReason {
    Requested,
    Exited,
    PluginDisabled,
    PluginRemoved,
}

/// How a region's process ended, when it ended on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionExitInfo {
    pub code: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}
