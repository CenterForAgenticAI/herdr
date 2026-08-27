//! Desktop regions — host-owned screen area outside any tab's pane tree.
//!
//! A *region* is a rectangle of the desktop frame that Herdr reserves for one
//! hosted terminal program. Unlike a pane, it is not part of a tab's tiled
//! layout, so switching or closing a tab never destroys it.
//!
//! This module is pure data and geometry: it holds no terminal runtime and does
//! no I/O, so it is testable without PTYs. The `TerminalRuntime` for a region
//! lives in the shared runtime registry, exactly as panes and the popup do.
//!
//! Phase 1 lands the model, frame subtraction, config, and persistence. The
//! slot-lifecycle API below is exercised by this module's tests and by view
//! computation; the open/close/focus/resize callers arrive with the region API,
//! so the non-test build does not reach all of it yet.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::terminal::TerminalId;

/// Columns the tab surface keeps when a right-anchored region is shown.
///
/// Below this the region is dropped for the frame instead of squeezing the
/// pane tree into an unusable width.
pub const MIN_TAB_SURFACE_WIDTH: u16 = 20;

/// Rows the frame needs before a region can show bordered content.
pub const MIN_REGION_HEIGHT: u16 = 3;

/// Smallest size a region may ever occupy along its anchored axis.
pub const REGION_SIZE_FLOOR: u16 = 4;

/// Largest size a region may ever occupy along its anchored axis.
pub const REGION_SIZE_CEILING: u16 = 400;

/// Size used when neither the manifest nor user config picks one.
pub const DEFAULT_REGION_SIZE: u16 = 32;

/// Lower bound used when a manifest declares no `min_size`.
pub const DEFAULT_REGION_MIN_SIZE: u16 = 8;

/// Upper bound used when a manifest declares no `max_size`.
pub const DEFAULT_REGION_MAX_SIZE: u16 = 200;

/// Which frame edge a region occupies.
///
/// Phase 1 ships `right` only. The type exists so other edges are a variant
/// away rather than a redesign.
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
pub enum RegionAnchor {
    #[default]
    Right,
}

/// What a region's lifetime is bound to.
///
/// Phase 1 ships `session` only: one instance for the whole session, shared by
/// every Space and tab.
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

static NEXT_REGION_ID: AtomicU64 = AtomicU64::new(1);

/// Stable public identifier for one live region.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RegionId(String);

impl RegionId {
    pub fn alloc() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_REGION_ID.fetch_add(1, Ordering::Relaxed);
        Self(format!("region_{micros:x}{counter:x}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RegionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Server-owned size bounds for one region.
///
/// Bounds can only be built through [`RegionSizeBounds::new`], which clamps
/// them into the absolute floor/ceiling. A caller therefore cannot widen its
/// own limits, zero a region, or make one consume the whole frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionSizeBounds {
    min: u16,
    max: u16,
}

impl RegionSizeBounds {
    pub fn new(min: Option<u16>, max: Option<u16>) -> Self {
        let min = min
            .unwrap_or(DEFAULT_REGION_MIN_SIZE)
            .clamp(REGION_SIZE_FLOOR, REGION_SIZE_CEILING);
        let max = max
            .unwrap_or(DEFAULT_REGION_MAX_SIZE)
            .clamp(REGION_SIZE_FLOOR, REGION_SIZE_CEILING)
            .max(min);
        Self { min, max }
    }

    pub fn min(self) -> u16 {
        self.min
    }

    pub fn max(self) -> u16 {
        self.max
    }

    /// Clamp a requested size into these bounds.
    pub fn clamp(self, size: u16) -> u16 {
        size.clamp(self.min, self.max)
    }
}

impl Default for RegionSizeBounds {
    fn default() -> Self {
        Self::new(None, None)
    }
}

/// One live region: declarative state only, never a runtime handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionInstance {
    pub region_id: RegionId,
    pub plugin_id: String,
    pub entrypoint: String,
    pub terminal_id: TerminalId,
    pub anchor: RegionAnchor,
    pub scope: RegionScope,
    pub title: String,
    /// Terminal cells along the anchored axis: columns for `right`.
    size: u16,
    bounds: RegionSizeBounds,
    pub visible: bool,
}

impl RegionInstance {
    pub fn new(
        region_id: RegionId,
        plugin_id: String,
        entrypoint: String,
        terminal_id: TerminalId,
        anchor: RegionAnchor,
        scope: RegionScope,
        title: String,
        size: u16,
        bounds: RegionSizeBounds,
        visible: bool,
    ) -> Self {
        Self {
            region_id,
            plugin_id,
            entrypoint,
            terminal_id,
            anchor,
            scope,
            title,
            size: bounds.clamp(size),
            bounds,
            visible,
        }
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// Set the size, clamped into this region's server-owned bounds.
    ///
    /// Returns the size actually applied, which is never the caller's value
    /// when that value falls outside the bounds.
    pub fn set_size(&mut self, size: u16) -> u16 {
        self.size = self.bounds.clamp(size);
        self.size
    }
}

/// Why a region could not be opened into its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionSlotOccupied;

/// Every live region, keyed by slot.
///
/// The invariant is one region per (scope-instance, anchor). A second open
/// request for an occupied slot is an error rather than a silent replace: two
/// providers fighting over one edge has no correct resolution, and the failure
/// must be visible to the plugin author.
#[derive(Debug, Clone, Default)]
pub struct RegionState {
    session: BTreeMap<RegionAnchor, RegionInstance>,
}

impl RegionState {
    pub fn is_empty(&self) -> bool {
        self.session.is_empty()
    }

    pub fn len(&self) -> usize {
        self.session.len()
    }

    /// Claim a slot for a new region.
    pub fn insert(
        &mut self,
        instance: RegionInstance,
    ) -> Result<&RegionInstance, RegionSlotOccupied> {
        let anchor = instance.anchor;
        match instance.scope {
            RegionScope::Session => {
                if self.session.contains_key(&anchor) {
                    return Err(RegionSlotOccupied);
                }
                Ok(self.session.entry(anchor).or_insert(instance))
            }
        }
    }

    pub fn get(&self, anchor: RegionAnchor) -> Option<&RegionInstance> {
        self.session.get(&anchor)
    }

    pub fn get_mut(&mut self, anchor: RegionAnchor) -> Option<&mut RegionInstance> {
        self.session.get_mut(&anchor)
    }

    pub fn get_by_id(&self, region_id: &str) -> Option<&RegionInstance> {
        self.session
            .values()
            .find(|instance| instance.region_id.as_str() == region_id)
    }

    pub fn remove_by_id(&mut self, region_id: &str) -> Option<RegionInstance> {
        let anchor = self
            .session
            .iter()
            .find(|(_, instance)| instance.region_id.as_str() == region_id)
            .map(|(anchor, _)| *anchor)?;
        self.session.remove(&anchor)
    }

    /// Remove every region owned by one plugin, returning them in slot order.
    pub fn remove_by_plugin(&mut self, plugin_id: &str) -> Vec<RegionInstance> {
        let anchors: Vec<RegionAnchor> = self
            .session
            .iter()
            .filter(|(_, instance)| instance.plugin_id == plugin_id)
            .map(|(anchor, _)| *anchor)
            .collect();
        anchors
            .into_iter()
            .filter_map(|anchor| self.session.remove(&anchor))
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RegionInstance> {
        self.session.values()
    }

    /// Size of the shown region at one anchor, by key rather than by scan.
    ///
    /// Returns `None` when the slot is empty or the region is hidden, so a
    /// hidden region costs no layout and no resize work.
    pub fn shown_size(&self, anchor: RegionAnchor) -> Option<u16> {
        self.session
            .get(&anchor)
            .filter(|instance| instance.visible)
            .map(RegionInstance::size)
    }
}

/// The desktop frame after regions have been subtracted from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionLayout {
    /// Area reserved for the right-anchored region, when it is shown.
    pub right: Option<Rect>,
    /// What is left for the tab bar and the tab's pane tree.
    pub remaining: Rect,
}

/// Subtract the desktop regions from `main_area`.
///
/// `right` is the requested width of the right-anchored region in columns, or
/// `None` when no right region is open or it is hidden. The region is dropped
/// for this frame when the tab surface would be left below
/// [`MIN_TAB_SURFACE_WIDTH`] columns or the frame is shorter than
/// [`MIN_REGION_HEIGHT`], so content is never clipped into an unusable size and
/// the region returns when space does.
///
/// `main_area` is the frame that is left after the built-in sidebar has been
/// subtracted. The right region takes the full height of that remainder, so the
/// tab bar is computed from the narrowed rectangle and never runs under it.
pub fn layout_regions(main_area: Rect, right: Option<u16>) -> RegionLayout {
    let hidden = RegionLayout {
        right: None,
        remaining: main_area,
    };
    let Some(width) = right else {
        return hidden;
    };
    if main_area.height < MIN_REGION_HEIGHT {
        return hidden;
    }
    let width = width.clamp(REGION_SIZE_FLOOR, REGION_SIZE_CEILING);
    if main_area.width < width.saturating_add(MIN_TAB_SURFACE_WIDTH) {
        return hidden;
    }

    let remaining_width = main_area.width - width;
    RegionLayout {
        right: Some(Rect::new(
            main_area.x + remaining_width,
            main_area.y,
            width,
            main_area.height,
        )),
        remaining: Rect::new(main_area.x, main_area.y, remaining_width, main_area.height),
    }
}

/// Content area inside a region's border chrome.
pub fn region_inner_rect(outer: Rect) -> Rect {
    Rect {
        x: outer.x.saturating_add(1),
        y: outer.y.saturating_add(1),
        width: outer.width.saturating_sub(2),
        height: outer.height.saturating_sub(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(anchor: RegionAnchor, size: u16, visible: bool) -> RegionInstance {
        RegionInstance::new(
            RegionId::alloc(),
            "explorer".into(),
            "sidebar".into(),
            TerminalId::alloc(),
            anchor,
            RegionScope::Session,
            "Explorer".into(),
            size,
            RegionSizeBounds::new(Some(18), Some(60)),
            visible,
        )
    }

    #[test]
    fn right_region_subtracts_from_the_tab_area_only() {
        let main_area = Rect::new(26, 0, 74, 20);

        let layout = layout_regions(main_area, Some(32));

        assert_eq!(layout.right, Some(Rect::new(68, 0, 32, 20)));
        assert_eq!(layout.remaining, Rect::new(26, 0, 42, 20));
    }

    #[test]
    fn right_region_spans_the_full_height_of_the_remainder() {
        let layout = layout_regions(Rect::new(0, 0, 120, 40), Some(30));

        let right = layout.right.expect("region shown");
        assert_eq!(right.y, 0);
        assert_eq!(right.height, 40);
        assert_eq!(right.x + right.width, 120);
    }

    #[test]
    fn no_region_leaves_geometry_untouched() {
        let main_area = Rect::new(4, 1, 80, 20);

        let layout = layout_regions(main_area, None);

        assert_eq!(layout.right, None);
        assert_eq!(layout.remaining, main_area);
    }

    #[test]
    fn region_hides_when_the_tab_surface_would_be_unusable() {
        // 32 + MIN_TAB_SURFACE_WIDTH is the first width that still fits.
        let fits = 32 + MIN_TAB_SURFACE_WIDTH;

        let shown = layout_regions(Rect::new(0, 0, fits, 20), Some(32));
        let hidden = layout_regions(Rect::new(0, 0, fits - 1, 20), Some(32));

        assert!(shown.right.is_some());
        assert_eq!(hidden.right, None);
        assert_eq!(hidden.remaining.width, fits - 1);
    }

    #[test]
    fn region_returns_when_the_frame_grows_back() {
        let narrow = layout_regions(Rect::new(0, 0, 40, 20), Some(32));
        let wide = layout_regions(Rect::new(0, 0, 120, 20), Some(32));

        assert_eq!(narrow.right, None);
        assert_eq!(wide.right, Some(Rect::new(88, 0, 32, 20)));
    }

    #[test]
    fn region_hides_on_a_frame_too_short_for_bordered_content() {
        let layout = layout_regions(Rect::new(0, 0, 120, MIN_REGION_HEIGHT - 1), Some(32));

        assert_eq!(layout.right, None);
    }

    #[test]
    fn layout_composes_deterministically_across_frame_sizes() {
        for width in [60_u16, 80, 100, 133, 240] {
            for height in [3_u16, 20, 51] {
                let main_area = Rect::new(3, 1, width, height);

                let layout = layout_regions(main_area, Some(32));

                match layout.right {
                    Some(right) => {
                        assert_eq!(right.width, 32);
                        assert_eq!(layout.remaining.width + right.width, width);
                        assert_eq!(layout.remaining.x, main_area.x);
                        assert_eq!(right.x, layout.remaining.x + layout.remaining.width);
                        assert_eq!(right.height, height);
                        assert!(layout.remaining.width >= MIN_TAB_SURFACE_WIDTH);
                    }
                    None => assert_eq!(layout.remaining, main_area),
                }
            }
        }
    }

    #[test]
    fn a_hostile_size_cannot_consume_the_whole_frame() {
        let main_area = Rect::new(0, 0, 120, 20);

        let layout = layout_regions(main_area, Some(u16::MAX));

        assert_eq!(layout.right, None);
        assert_eq!(layout.remaining, main_area);
    }

    #[test]
    fn bounds_clamp_a_caller_supplied_size() {
        let bounds = RegionSizeBounds::new(Some(18), Some(60));

        assert_eq!(bounds.clamp(0), 18);
        assert_eq!(bounds.clamp(9), 18);
        assert_eq!(bounds.clamp(32), 32);
        assert_eq!(bounds.clamp(u16::MAX), 60);
    }

    #[test]
    fn bounds_cannot_be_widened_past_the_absolute_limits() {
        let bounds = RegionSizeBounds::new(Some(0), Some(u16::MAX));

        assert_eq!(bounds.min(), REGION_SIZE_FLOOR);
        assert_eq!(bounds.max(), REGION_SIZE_CEILING);
        assert_eq!(bounds.clamp(0), REGION_SIZE_FLOOR);
        assert_eq!(bounds.clamp(u16::MAX), REGION_SIZE_CEILING);
    }

    #[test]
    fn inverted_bounds_do_not_produce_an_empty_range() {
        let bounds = RegionSizeBounds::new(Some(60), Some(10));

        assert_eq!(bounds.min(), 60);
        assert_eq!(bounds.max(), 60);
        assert_eq!(bounds.clamp(10), 60);
    }

    #[test]
    fn instance_size_is_clamped_on_construction_and_on_resize() {
        let mut region = instance(RegionAnchor::Right, u16::MAX, true);
        assert_eq!(region.size(), 60);

        assert_eq!(region.set_size(1), 18);
        assert_eq!(region.size(), 18);
        assert_eq!(region.set_size(40), 40);
    }

    #[test]
    fn a_second_open_for_an_occupied_slot_is_refused() {
        let mut state = RegionState::default();
        let first = instance(RegionAnchor::Right, 32, true);
        let first_id = first.region_id.clone();
        state.insert(first).expect("first region claims the slot");

        let second = state.insert(instance(RegionAnchor::Right, 20, true));

        assert_eq!(second.err(), Some(RegionSlotOccupied));
        assert_eq!(state.len(), 1);
        assert_eq!(
            state.get(RegionAnchor::Right).map(|r| r.region_id.clone()),
            Some(first_id),
            "the occupying region must not be replaced"
        );
    }

    #[test]
    fn slot_is_released_on_close_and_can_be_claimed_again() {
        let mut state = RegionState::default();
        state
            .insert(instance(RegionAnchor::Right, 32, true))
            .unwrap();
        let region_id = state.get(RegionAnchor::Right).unwrap().region_id.clone();

        let removed = state.remove_by_id(region_id.as_str());

        assert!(removed.is_some());
        assert!(state.is_empty());
        assert!(state
            .insert(instance(RegionAnchor::Right, 32, true))
            .is_ok());
    }

    #[test]
    fn hidden_regions_report_no_size_so_they_cost_no_layout() {
        let mut state = RegionState::default();
        state
            .insert(instance(RegionAnchor::Right, 32, false))
            .unwrap();

        assert_eq!(state.shown_size(RegionAnchor::Right), None);

        state.get_mut(RegionAnchor::Right).unwrap().visible = true;

        assert_eq!(state.shown_size(RegionAnchor::Right), Some(32));
    }

    #[test]
    fn removing_by_plugin_takes_only_that_plugins_regions() {
        let mut state = RegionState::default();
        state
            .insert(instance(RegionAnchor::Right, 32, true))
            .unwrap();

        assert!(state.remove_by_plugin("other").is_empty());
        assert_eq!(state.len(), 1);
        assert_eq!(state.remove_by_plugin("explorer").len(), 1);
        assert!(state.is_empty());
    }

    #[test]
    fn inner_rect_never_underflows_on_a_tiny_outer_rect() {
        assert_eq!(
            region_inner_rect(Rect::new(5, 2, 0, 0)),
            Rect::new(6, 3, 0, 0)
        );
        assert_eq!(
            region_inner_rect(Rect::new(0, 0, 32, 20)),
            Rect::new(1, 1, 30, 18)
        );
    }
}
