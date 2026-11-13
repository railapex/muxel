//! What an offscreen scene holds fixed. The app reads the real clock and
//! animates; a test that draws a scene fixes the clock, holds animation still
//! and records where each named region was drawn, so two renders of one scene
//! give the same pixels and a frame can be cropped to one region.
//!
//! The switches are thread-local and exist only in test builds: the scene
//! runner draws on the thread that sets them. Their setters are compiled only
//! where the runner is, in Windows test builds.

use chrono::{DateTime, Local};

#[cfg(test)]
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};
#[cfg(test)]
use std::cell::{Cell, RefCell};
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
thread_local! {
    static FIXED_NOW: Cell<Option<DateTime<chrono::Utc>>> = const { Cell::new(None) };
    static FROZEN_INSTANT: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
    static STILL: Cell<bool> = const { Cell::new(false) };
    static REGIONS: RefCell<Option<HashMap<&'static str, Bounds<Pixels>>>> =
        const { RefCell::new(None) };
}

/// The wall clock, as the labels that show an age or a date read it.
pub(crate) fn now_local() -> DateTime<Local> {
    #[cfg(test)]
    if let Some(now) = FIXED_NOW.with(Cell::get) {
        return now.with_timezone(&Local);
    }
    Local::now()
}

/// The monotonic clock, as a countdown's label reads it.
pub(crate) fn now_instant() -> std::time::Instant {
    #[cfg(test)]
    if let Some(now) = FROZEN_INSTANT.with(Cell::get) {
        return now;
    }
    std::time::Instant::now()
}

/// The easing for an activity spinner: `easing`, or a constant while animation
/// is held still, which draws the spinner at rest on every frame.
pub(crate) fn spinner_easing(easing: fn(f32) -> f32) -> fn(f32) -> f32 {
    #[cfg(test)]
    if STILL.with(Cell::get) {
        return |_| 0.0;
    }
    easing
}

/// `element`, recorded under `name` while a scene is recording regions.
#[cfg(test)]
pub(crate) fn region(name: &'static str, element: AnyElement) -> AnyElement {
    if REGIONS.with(|regions| regions.borrow().is_some()) {
        return SceneRegion {
            inner: element,
            name,
        }
        .into_any_element();
    }
    element
}

/// Fix the wall clock at `now`, or release it with `None`.
#[cfg(all(test, windows))]
pub(crate) fn fix_clock(now: Option<DateTime<chrono::Utc>>) {
    FIXED_NOW.with(|fixed| fixed.set(now));
}

/// Stop the monotonic clock at `now`, or let it run with `None`.
#[cfg(all(test, windows))]
pub(crate) fn freeze_instant(now: Option<std::time::Instant>) {
    FROZEN_INSTANT.with(|frozen| frozen.set(now));
}

/// Hold every activity spinner at rest, or let them turn again.
#[cfg(all(test, windows))]
pub(crate) fn hold_still(still: bool) {
    STILL.with(|held| held.set(still));
}

/// Start recording where named regions are drawn; elements built after this
/// are wrapped, and each draw replaces a region's bounds.
#[cfg(all(test, windows))]
pub(crate) fn record_regions() {
    REGIONS.with(|regions| *regions.borrow_mut() = Some(HashMap::new()));
}

/// Where `name` was last drawn, in window pixels.
#[cfg(all(test, windows))]
pub(crate) fn region_bounds(name: &str) -> Option<Bounds<Pixels>> {
    REGIONS.with(|regions| regions.borrow().as_ref()?.get(name).copied())
}

/// Every region recorded so far with where it was last drawn, by name.
#[cfg(all(test, windows))]
pub(crate) fn regions() -> Vec<(&'static str, Bounds<Pixels>)> {
    REGIONS.with(|regions| {
        let mut all: Vec<_> = regions
            .borrow()
            .as_ref()
            .map(|map| map.iter().map(|(name, bounds)| (*name, *bounds)).collect())
            .unwrap_or_default();
        all.sort_unstable_by_key(|(name, _)| *name);
        all
    })
}

/// Lays its child out unchanged and notes the bounds it was given.
#[cfg(test)]
struct SceneRegion {
    inner: AnyElement,
    name: &'static str,
}

#[cfg(test)]
impl IntoElement for SceneRegion {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
impl Element for SceneRegion {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.inner.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        REGIONS.with(|regions| {
            if let Some(map) = regions.borrow_mut().as_mut() {
                map.insert(self.name, bounds);
            }
        });
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(window, cx);
    }
}
