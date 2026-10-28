//! Optional desktop-region projection, surface and viewport messages.
//!
//! Every message rides the frozen `EndpointControl { kind, data }` variant. A server sends
//! region traffic only to a client whose hello set `regions`, and a client sends viewports
//! and region input only to a server that advertised [`CAPABILITY`]. Unknown kinds are
//! ignored on both sides, so older peers never see a change.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

use super::{FrameData, ServerMessage, MAX_FRAME_SIZE};

pub(crate) const CAPABILITY: &str = "region_surface";
/// Server to client, JSON [`RegionProjection`].
pub(crate) const PROJECTION_KIND: &str = "endpoint.regions.v1";
/// Server to client, base64 (no padding) of bincode [`RegionSurface`].
pub(crate) const SURFACE_KIND: &str = "endpoint.region-surface.v1";
/// Client to server, JSON [`RegionViewport`].
pub(crate) const VIEWPORT_KIND: &str = "endpoint.region-viewport.v1";

const MAX_GRID_DIMENSION: u16 = 4096;
const MAX_GRID_CELLS: usize = 1_000_000;

/// The regions one server holds, as a client needs them for layout and input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegionProjection {
    pub(crate) boot_id: String,
    pub(crate) revision: u64,
    #[serde(default)]
    pub(crate) regions: Vec<ClientRegion>,
    #[serde(default)]
    pub(crate) focused: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClientRegion {
    pub(crate) region_id: String,
    pub(crate) terminal_id: String,
    pub(crate) plugin_id: String,
    pub(crate) entrypoint: String,
    pub(crate) title: String,
    /// Edge name. Kept as text so an edge this client does not know is skipped, not fatal.
    pub(crate) edge: String,
    pub(crate) scope: String,
    pub(crate) size: u16,
    pub(crate) min_size: u16,
    pub(crate) max_size: u16,
    pub(crate) visible: bool,
    pub(crate) focused: bool,
    /// The region terminal asked for mouse reports.
    #[serde(default)]
    pub(crate) mouse_reporting: bool,
}

// This binary layout is frozen with SURFACE_KIND, independently of the v1 core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegionSurface {
    pub(crate) region_id: String,
    pub(crate) terminal_id: String,
    pub(crate) frame: FrameData,
}

/// Inner size a client drew for a region. `cols == 0` means hidden on that client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegionViewport {
    pub(crate) region_id: String,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

pub(crate) fn projection_message(
    projection: &RegionProjection,
) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: PROJECTION_KIND.into(),
        data: serde_json::to_string(projection)?,
    })
}

pub(crate) fn decode_projection(data: &str) -> Option<RegionProjection> {
    serde_json::from_str(data).ok()
}

pub(crate) fn surface_message(surface: &RegionSurface) -> Result<ServerMessage, String> {
    let bytes = bincode::serde::encode_to_vec(surface, bincode::config::standard())
        .map_err(|error| error.to_string())?;
    Ok(ServerMessage::EndpointControl {
        kind: SURFACE_KIND.into(),
        data: STANDARD_NO_PAD.encode(bytes),
    })
}

/// Decode a region surface. Oversized payloads and frames whose cell count does not match
/// their size are rejected before they reach the compositor.
pub(crate) fn decode_surface(data: &str) -> Result<RegionSurface, String> {
    let max_encoded = base64::encoded_len(MAX_FRAME_SIZE, false).unwrap_or(usize::MAX);
    if data.len() > max_encoded {
        return Err("region surface exceeds the frame limit".into());
    }
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| error.to_string())?;
    let (surface, consumed): (RegionSurface, usize) = bincode::serde::decode_from_slice(
        &bytes,
        bincode::config::standard().with_limit::<MAX_FRAME_SIZE>(),
    )
    .map_err(|error| error.to_string())?;
    if consumed != bytes.len() {
        return Err("trailing region surface bytes".into());
    }
    let frame = &surface.frame;
    if frame.width > MAX_GRID_DIMENSION
        || frame.height > MAX_GRID_DIMENSION
        || usize::from(frame.width) * usize::from(frame.height) > MAX_GRID_CELLS
        || frame.cells.len() != usize::from(frame.width) * usize::from(frame.height)
    {
        return Err("region surface grid does not match its size".into());
    }
    Ok(surface)
}

pub(crate) fn viewport_data(viewport: &RegionViewport) -> serde_json::Result<String> {
    serde_json::to_string(viewport)
}

pub(crate) fn decode_viewport(data: &str) -> Option<RegionViewport> {
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn sample_surface() -> RegionSurface {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer[(0, 0)].set_symbol("r");
        buffer[(3, 1)].set_symbol("g");
        RegionSurface {
            region_id: "region_1".into(),
            terminal_id: "term_1".into(),
            frame: FrameData::from_ratatui_buffer(
                &buffer,
                Some(crate::protocol::CursorState {
                    x: 1,
                    y: 1,
                    visible: true,
                    shape: Default::default(),
                }),
            ),
        }
    }

    fn control_data(message: ServerMessage) -> (String, String) {
        match message {
            ServerMessage::EndpointControl { kind, data } => (kind, data),
            other => panic!("expected endpoint control, got {other:?}"),
        }
    }

    #[test]
    fn region_surface_round_trips_through_endpoint_control() {
        let surface = sample_surface();
        let (kind, data) = control_data(surface_message(&surface).unwrap());
        assert_eq!(kind, "endpoint.region-surface.v1");
        assert_eq!(decode_surface(&data).unwrap(), surface);
    }

    #[test]
    fn region_surface_v1_binary_fixture_is_frozen() {
        use sha2::{Digest, Sha256};
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, &surface_message(&sample_surface()).unwrap())
            .unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(bytes)),
            "695910daf340627cd79d5d1f952f1b05ff19818403cc1f5bba36ead1dfcebfbd"
        );
    }

    #[test]
    fn region_surface_rejects_a_grid_that_does_not_match_its_size() {
        let mut surface = sample_surface();
        surface.frame.width = 40;
        let (_, data) = control_data(surface_message(&surface).unwrap());
        assert!(decode_surface(&data).is_err());
    }

    #[test]
    fn region_surface_rejects_oversized_payloads_before_decoding() {
        let data = "A".repeat(base64::encoded_len(MAX_FRAME_SIZE, false).unwrap() + 1);
        assert_eq!(
            decode_surface(&data).unwrap_err(),
            "region surface exceeds the frame limit"
        );
    }

    #[test]
    fn projection_round_trips_and_tolerates_unknown_edges() {
        let projection = RegionProjection {
            boot_id: "boot".into(),
            revision: 3,
            regions: vec![ClientRegion {
                region_id: "region_1".into(),
                terminal_id: "term_1".into(),
                plugin_id: "board".into(),
                entrypoint: "board".into(),
                title: "Board".into(),
                edge: "left".into(),
                scope: "session".into(),
                size: 36,
                min_size: 12,
                max_size: 60,
                visible: true,
                focused: false,
                mouse_reporting: false,
            }],
            focused: None,
        };
        let (kind, data) = control_data(projection_message(&projection).unwrap());
        assert_eq!(kind, "endpoint.regions.v1");
        assert_eq!(decode_projection(&data), Some(projection));
    }

    #[test]
    fn projection_without_optional_fields_decodes() {
        let projection = decode_projection(r#"{"boot_id":"b","revision":1}"#).unwrap();
        assert!(projection.regions.is_empty());
        assert_eq!(projection.focused, None);
    }

    #[test]
    fn viewport_round_trips() {
        let viewport = RegionViewport {
            region_id: "region_1".into(),
            cols: 34,
            rows: 22,
        };
        assert_eq!(
            decode_viewport(&viewport_data(&viewport).unwrap()),
            Some(viewport)
        );
        assert_eq!(decode_viewport("{\"region_id\":7}"), None);
    }
}
