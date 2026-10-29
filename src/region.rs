//! Desktop regions: plugin-declared strips at one edge of the client frame.
//!
//! A region sits outside every tab's pane tree and hosts one terminal. This module is the
//! pure half: identity, size bounds, slot bookkeeping, and frame geometry. It performs no
//! I/O and owns no runtime; `crate::app::region` spawns and tears down the terminal.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::layout::PaneId;
use crate::terminal::TerminalId;

/// The tab area never shrinks below this width to make room for a region.
pub(crate) const MIN_TAB_SURFACE_WIDTH: u16 = 20;
/// A right region is hidden for a frame shorter than this.
pub(crate) const MIN_REGION_HEIGHT: u16 = 3;
/// The tab area never shrinks below this height to make room for a bottom region.
pub(crate) const MIN_TAB_SURFACE_HEIGHT: u16 = 6;
/// Absolute lower bound for any region size, including manifest and config bounds.
pub(crate) const REGION_SIZE_FLOOR: u16 = 4;
/// Absolute upper bound for any region size, including manifest and config bounds.
pub(crate) const REGION_SIZE_CEILING: u16 = 400;
pub(crate) const DEFAULT_REGION_SIZE: u16 = 32;
pub(crate) const DEFAULT_REGION_MIN_SIZE: u16 = 8;
pub(crate) const DEFAULT_REGION_MAX_SIZE: u16 = 200;

/// Frame edge a region occupies.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RegionEdge {
    #[default]
    Right,
    Bottom,
}

impl RegionEdge {
    pub(crate) const SUPPORTED: &'static [&'static str] = &["right", "bottom"];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Bottom => "bottom",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "right" => Some(Self::Right),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
}

/// How widely one region instance is shared. Phase 1 supports only `session`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RegionScope {
    #[default]
    Session,
}

impl RegionScope {
    pub(crate) const SUPPORTED: &'static [&'static str] = &["session"];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

static NEXT_REGION_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque region identity, `region_<hex>`. Never derived from a pane or terminal id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RegionId(String);

impl RegionId {
    pub(crate) fn alloc() -> Self {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_REGION_ID.fetch_add(1, Ordering::Relaxed);
        Self(format!("region_{micros:x}{counter:x}"))
    }

    #[cfg(test)]
    pub(crate) fn from_test(value: &str) -> Self {
        Self(value.to_string())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Inclusive size bounds, always within `REGION_SIZE_FLOOR..=REGION_SIZE_CEILING` with
/// `min <= max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionSizeBounds {
    min: u16,
    max: u16,
}

impl Default for RegionSizeBounds {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl RegionSizeBounds {
    /// Build bounds from optional manifest values. Each value is clamped to the global
    /// floor and ceiling; an inverted pair collapses to `min`.
    pub(crate) fn new(min: Option<u16>, max: Option<u16>) -> Self {
        let min = min
            .unwrap_or(DEFAULT_REGION_MIN_SIZE)
            .clamp(REGION_SIZE_FLOOR, REGION_SIZE_CEILING);
        let max = max
            .unwrap_or(DEFAULT_REGION_MAX_SIZE.max(min))
            .clamp(REGION_SIZE_FLOOR, REGION_SIZE_CEILING)
            .max(min);
        Self { min, max }
    }

    pub(crate) fn min(self) -> u16 {
        self.min
    }

    pub(crate) fn max(self) -> u16 {
        self.max
    }

    pub(crate) fn clamp(self, size: u16) -> u16 {
        size.clamp(self.min, self.max)
    }
}

/// One live region slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionInstance {
    pub region_id: RegionId,
    pub plugin_id: String,
    pub entrypoint: String,
    pub terminal_id: TerminalId,
    /// Exists only to route `PaneDied`; a region has no layout entry.
    pub pane_id: PaneId,
    pub edge: RegionEdge,
    pub scope: RegionScope,
    pub title: String,
    pub size: u16,
    pub bounds: RegionSizeBounds,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionSlotOccupied {
    pub edge: RegionEdge,
    pub scope: RegionScope,
    pub occupant: RegionId,
}

/// All region slots. Phase 1 has only session scope, one slot per edge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RegionState {
    session: BTreeMap<RegionEdge, RegionInstance>,
}

impl RegionState {
    /// Insert into an empty slot. An occupied slot is never replaced.
    pub(crate) fn insert(&mut self, region: RegionInstance) -> Result<(), RegionSlotOccupied> {
        if let Some(occupant) = self.session.get(&region.edge) {
            return Err(RegionSlotOccupied {
                edge: region.edge,
                scope: region.scope,
                occupant: occupant.region_id.clone(),
            });
        }
        self.session.insert(region.edge, region);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.session.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &RegionInstance> {
        self.session.values()
    }

    pub(crate) fn get(&self, edge: RegionEdge) -> Option<&RegionInstance> {
        self.session.get(&edge)
    }

    pub(crate) fn get_by_id(&self, region_id: &str) -> Option<&RegionInstance> {
        self.session
            .values()
            .find(|region| region.region_id.as_str() == region_id)
    }

    pub(crate) fn get_mut_by_id(&mut self, region_id: &str) -> Option<&mut RegionInstance> {
        self.session
            .values_mut()
            .find(|region| region.region_id.as_str() == region_id)
    }

    pub(crate) fn remove_by_id(&mut self, region_id: &str) -> Option<RegionInstance> {
        let edge = self.get_by_id(region_id)?.edge;
        self.session.remove(&edge)
    }

    /// Remove every region a plugin owns. Other plugins' regions stay.
    #[cfg(test)]
    pub(crate) fn remove_by_plugin(&mut self, plugin_id: &str) -> Vec<RegionInstance> {
        let edges: Vec<RegionEdge> = self
            .session
            .iter()
            .filter(|(_, region)| region.plugin_id == plugin_id)
            .map(|(edge, _)| *edge)
            .collect();
        edges
            .into_iter()
            .filter_map(|edge| self.session.remove(&edge))
            .collect()
    }

    pub(crate) fn find_by_pane(&self, pane_id: PaneId) -> Option<&RegionInstance> {
        self.session
            .values()
            .find(|region| region.pane_id == pane_id)
    }

    pub(crate) fn find_by_terminal(&self, terminal_id: &str) -> Option<&RegionInstance> {
        self.session
            .values()
            .find(|region| region.terminal_id.as_str() == terminal_id)
    }

    /// Size the edge asks for, when a visible region occupies it.
    #[cfg(test)]
    pub(crate) fn shown_size(&self, edge: RegionEdge) -> Option<u16> {
        self.session
            .get(&edge)
            .filter(|region| region.visible)
            .map(|region| region.size)
    }
}

/// Sizes the shown regions ask for, one per edge. `None` or zero means no region there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RegionSizes {
    pub right: Option<u16>,
    pub bottom: Option<u16>,
}

/// Frame split between region strips and the remaining tab area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegionLayout {
    /// Outer rect of the right region, or `None` when absent or hidden for this frame.
    pub right: Option<Rect>,
    /// Outer rect of the bottom region, or `None` when absent or hidden for this frame.
    pub bottom: Option<Rect>,
    /// Area left for the tab bar and pane surface.
    pub remaining: Rect,
}

/// Take the right region's width from the right of `main` at full height, then the bottom
/// region's height from the bottom of what is left. The bottom region therefore spans only
/// the tab area's width and never runs under the right region.
///
/// A region is hidden for this frame (not shrunk, not closed) when it would leave the tab
/// area narrower than `MIN_TAB_SURFACE_WIDTH` or shorter than `MIN_TAB_SURFACE_HEIGHT`, or
/// when the frame is shorter than `MIN_REGION_HEIGHT`. Each edge is decided on its own, so
/// one oversized region never hides the other. A hostile size can never consume the frame.
pub(crate) fn layout_regions(main: Rect, sizes: RegionSizes) -> RegionLayout {
    let mut remaining = main;
    let right = sizes
        .right
        .filter(|size| *size > 0 && main.height >= MIN_REGION_HEIGHT)
        .and_then(|size| {
            let width = remaining.width.checked_sub(size)?;
            (width >= MIN_TAB_SURFACE_WIDTH).then(|| {
                let rect = Rect::new(
                    remaining.x.saturating_add(width),
                    remaining.y,
                    size,
                    remaining.height,
                );
                remaining.width = width;
                rect
            })
        });
    let bottom = sizes.bottom.filter(|size| *size > 0).and_then(|size| {
        let height = remaining.height.checked_sub(size)?;
        (height >= MIN_TAB_SURFACE_HEIGHT).then(|| {
            let rect = Rect::new(
                remaining.x,
                remaining.y.saturating_add(height),
                remaining.width,
                size,
            );
            remaining.height = height;
            rect
        })
    });
    RegionLayout {
        right,
        bottom,
        remaining,
    }
}

/// Content rect inside the region's one-cell border.
pub(crate) fn region_inner(outer: Rect) -> Rect {
    Rect::new(
        outer.x.saturating_add(1),
        outer.y.saturating_add(1),
        outer.width.saturating_sub(2),
        outer.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: &str, plugin: &str, edge: RegionEdge) -> RegionInstance {
        RegionInstance {
            region_id: RegionId::from_test(id),
            plugin_id: plugin.into(),
            entrypoint: "notes".into(),
            terminal_id: TerminalId::alloc(),
            pane_id: PaneId::alloc(),
            edge,
            scope: RegionScope::Session,
            title: "Notes".into(),
            size: 30,
            bounds: RegionSizeBounds::default(),
            visible: true,
        }
    }

    fn right(size: u16) -> RegionSizes {
        RegionSizes {
            right: Some(size),
            bottom: None,
        }
    }

    fn bottom(size: u16) -> RegionSizes {
        RegionSizes {
            right: None,
            bottom: Some(size),
        }
    }

    #[test]
    fn layout_takes_size_from_the_bottom_at_full_width() {
        let main = Rect::new(26, 0, 100, 40);
        let layout = layout_regions(main, bottom(12));
        assert_eq!(layout.bottom, Some(Rect::new(26, 28, 100, 12)));
        assert_eq!(layout.right, None);
        assert_eq!(layout.remaining, Rect::new(26, 0, 100, 28));
    }

    #[test]
    fn bottom_region_spans_only_the_tab_area_beside_a_right_region() {
        let main = Rect::new(26, 0, 100, 40);
        let layout = layout_regions(
            main,
            RegionSizes {
                right: Some(30),
                bottom: Some(12),
            },
        );
        assert_eq!(layout.right, Some(Rect::new(96, 0, 30, 40)));
        assert_eq!(layout.bottom, Some(Rect::new(26, 28, 70, 12)));
        assert_eq!(layout.remaining, Rect::new(26, 0, 70, 28));
    }

    #[test]
    fn layout_hides_bottom_exactly_when_tab_area_would_drop_below_minimum_height() {
        let fits = Rect::new(0, 0, 80, 12 + MIN_TAB_SURFACE_HEIGHT);
        let layout = layout_regions(fits, bottom(12));
        assert_eq!(layout.remaining.height, MIN_TAB_SURFACE_HEIGHT);
        assert!(layout.bottom.is_some());

        let one_short = Rect::new(0, 0, 80, 12 + MIN_TAB_SURFACE_HEIGHT - 1);
        let layout = layout_regions(one_short, bottom(12));
        assert_eq!(layout.bottom, None);
        assert_eq!(layout.remaining, one_short);
    }

    #[test]
    fn an_oversized_region_on_one_edge_does_not_hide_the_other() {
        let main = Rect::new(0, 0, 100, 40);
        let layout = layout_regions(
            main,
            RegionSizes {
                right: Some(u16::MAX),
                bottom: Some(10),
            },
        );
        assert_eq!(layout.right, None);
        assert_eq!(layout.bottom, Some(Rect::new(0, 30, 100, 10)));

        let layout = layout_regions(
            main,
            RegionSizes {
                right: Some(30),
                bottom: Some(u16::MAX),
            },
        );
        assert_eq!(layout.right, Some(Rect::new(70, 0, 30, 40)));
        assert_eq!(layout.bottom, None);
        assert_eq!(layout.remaining, Rect::new(0, 0, 70, 40));
    }

    #[test]
    fn hostile_bottom_size_cannot_consume_the_frame() {
        for main in [Rect::new(0, 0, 80, u16::MAX), Rect::new(0, 3, 80, 40)] {
            let layout = layout_regions(main, bottom(u16::MAX));
            assert_eq!(layout.bottom, None, "{main:?}");
            assert_eq!(layout.remaining, main);
        }
        let main = Rect::new(0, 0, 80, u16::MAX);
        let layout = layout_regions(main, bottom(u16::MAX - MIN_TAB_SURFACE_HEIGHT));
        assert_eq!(layout.remaining.height, MIN_TAB_SURFACE_HEIGHT);
    }

    #[test]
    fn layout_takes_size_from_the_right_at_full_height() {
        let main = Rect::new(26, 0, 100, 40);
        let layout = layout_regions(main, right(30));
        assert_eq!(layout.right, Some(Rect::new(96, 0, 30, 40)));
        assert_eq!(layout.remaining, Rect::new(26, 0, 70, 40));
    }

    #[test]
    fn layout_without_region_leaves_main_untouched() {
        let main = Rect::new(0, 1, 80, 24);
        assert_eq!(
            layout_regions(main, RegionSizes::default()),
            RegionLayout {
                right: None,
                bottom: None,
                remaining: main
            }
        );
        assert_eq!(layout_regions(main, right(0)).right, None);
        assert_eq!(layout_regions(main, right(0)).remaining, main);
    }

    #[test]
    fn layout_hides_exactly_when_tab_area_would_drop_below_minimum() {
        let fits = Rect::new(0, 0, 30 + MIN_TAB_SURFACE_WIDTH, 10);
        let layout = layout_regions(fits, right(30));
        assert_eq!(layout.remaining.width, MIN_TAB_SURFACE_WIDTH);
        assert!(layout.right.is_some());

        let one_short = Rect::new(0, 0, 30 + MIN_TAB_SURFACE_WIDTH - 1, 10);
        let layout = layout_regions(one_short, right(30));
        assert_eq!(layout.right, None);
        assert_eq!(layout.remaining, one_short);
    }

    #[test]
    fn layout_hides_exactly_below_minimum_height() {
        let fits = Rect::new(0, 0, 100, MIN_REGION_HEIGHT);
        assert!(layout_regions(fits, right(30)).right.is_some());
        let short = Rect::new(0, 0, 100, MIN_REGION_HEIGHT - 1);
        assert_eq!(layout_regions(short, right(30)).right, None);
        assert_eq!(layout_regions(short, right(30)).remaining, short);
    }

    #[test]
    fn hidden_region_returns_when_the_frame_regrows() {
        let narrow = Rect::new(0, 0, 40, 24);
        assert_eq!(layout_regions(narrow, right(30)).right, None);
        let wide = Rect::new(0, 0, 120, 24);
        assert_eq!(
            layout_regions(wide, right(30)).right,
            Some(Rect::new(90, 0, 30, 24))
        );
    }

    #[test]
    fn hostile_size_cannot_consume_the_frame() {
        for main in [
            Rect::new(0, 0, u16::MAX, 50),
            Rect::new(10, 0, 200, 50),
            Rect::new(0, 0, 0, 0),
        ] {
            let layout = layout_regions(main, right(u16::MAX));
            assert_eq!(layout.right, None, "{main:?}");
            assert_eq!(layout.remaining, main);
        }
        let main = Rect::new(0, 0, u16::MAX, 50);
        let layout = layout_regions(main, right(u16::MAX - 10));
        assert_eq!(layout.right, None);
        let layout = layout_regions(main, right(u16::MAX - MIN_TAB_SURFACE_WIDTH));
        assert_eq!(layout.remaining.width, MIN_TAB_SURFACE_WIDTH);
    }

    #[test]
    fn region_inner_strips_one_border_cell() {
        assert_eq!(
            region_inner(Rect::new(90, 0, 30, 24)),
            Rect::new(91, 1, 28, 22)
        );
        assert_eq!(region_inner(Rect::new(0, 0, 1, 1)).width, 0);
    }

    #[test]
    fn bounds_clamp_to_floor_and_ceiling() {
        let bounds = RegionSizeBounds::new(Some(0), Some(u16::MAX));
        assert_eq!(bounds.min(), REGION_SIZE_FLOOR);
        assert_eq!(bounds.max(), REGION_SIZE_CEILING);
        assert_eq!(bounds.clamp(0), REGION_SIZE_FLOOR);
        assert_eq!(bounds.clamp(u16::MAX), REGION_SIZE_CEILING);

        let defaults = RegionSizeBounds::default();
        assert_eq!(defaults.min(), DEFAULT_REGION_MIN_SIZE);
        assert_eq!(defaults.max(), DEFAULT_REGION_MAX_SIZE);
        assert_eq!(defaults.clamp(DEFAULT_REGION_SIZE), DEFAULT_REGION_SIZE);
    }

    #[test]
    fn inverted_bounds_collapse_to_min() {
        let bounds = RegionSizeBounds::new(Some(60), Some(20));
        assert_eq!(bounds.min(), 60);
        assert_eq!(bounds.max(), 60);
        assert_eq!(bounds.clamp(10), 60);
        assert_eq!(bounds.clamp(100), 60);

        let only_min_above_default_max = RegionSizeBounds::new(Some(300), None);
        assert_eq!(only_min_above_default_max.max(), 300);
    }

    #[test]
    fn occupied_slot_is_refused_and_never_replaced() {
        let mut state = RegionState::default();
        state
            .insert(region("region_a", "one", RegionEdge::Right))
            .unwrap();
        let err = state
            .insert(region("region_b", "two", RegionEdge::Right))
            .unwrap_err();
        assert_eq!(err.occupant.as_str(), "region_a");
        assert_eq!(
            state.get(RegionEdge::Right).unwrap().region_id.as_str(),
            "region_a"
        );
        assert_eq!(state.iter().count(), 1);
    }

    #[test]
    fn freed_slot_can_be_reused() {
        let mut state = RegionState::default();
        state
            .insert(region("region_a", "one", RegionEdge::Right))
            .unwrap();
        assert!(state.remove_by_id("region_a").is_some());
        assert!(state.remove_by_id("region_a").is_none());
        state
            .insert(region("region_b", "two", RegionEdge::Right))
            .unwrap();
        assert_eq!(
            state.get_by_id("region_b").unwrap().plugin_id,
            "two".to_string()
        );
    }

    #[test]
    fn remove_by_plugin_leaves_other_plugins_alone() {
        let mut state = RegionState::default();
        state
            .insert(region("region_a", "keep", RegionEdge::Right))
            .unwrap();
        assert!(state.remove_by_plugin("other").is_empty());
        assert_eq!(state.iter().count(), 1);
        let removed = state.remove_by_plugin("keep");
        assert_eq!(removed.len(), 1);
        assert!(state.is_empty());
    }

    #[test]
    fn lookups_by_pane_terminal_and_visibility() {
        let mut state = RegionState::default();
        let instance = region("region_a", "one", RegionEdge::Right);
        let pane_id = instance.pane_id;
        let terminal_id = instance.terminal_id.clone();
        state.insert(instance).unwrap();
        assert!(state.find_by_pane(pane_id).is_some());
        assert!(state.find_by_pane(PaneId::alloc()).is_none());
        assert!(state.find_by_terminal(terminal_id.as_str()).is_some());
        assert!(state.find_by_terminal("term_missing").is_none());
        assert_eq!(state.shown_size(RegionEdge::Right), Some(30));
        state.get_mut_by_id("region_a").unwrap().visible = false;
        assert_eq!(state.shown_size(RegionEdge::Right), None);
    }

    #[test]
    fn region_ids_are_unique_and_prefixed() {
        let a = RegionId::alloc();
        let b = RegionId::alloc();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("region_"));
    }

    #[test]
    fn edge_and_scope_parse_only_supported_values() {
        assert_eq!(RegionEdge::parse("right"), Some(RegionEdge::Right));
        assert_eq!(RegionEdge::parse("bottom"), Some(RegionEdge::Bottom));
        assert_eq!(RegionEdge::parse("left"), None);
        assert_eq!(RegionScope::parse("session"), Some(RegionScope::Session));
        assert_eq!(RegionScope::parse("tab"), None);
        assert_eq!(
            serde_json::to_string(&RegionEdge::Right).unwrap(),
            "\"right\""
        );
    }
}
