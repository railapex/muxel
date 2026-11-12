//! What a test that draws an offscreen scene records: where each named region
//! was drawn, so a frame can be cropped to one region.
//!
//! The record is thread-local and exists only in test builds: the scene
//! runner draws on the thread that starts it.

#[cfg(test)]
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};
#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
thread_local! {
    static REGIONS: RefCell<Option<HashMap<&'static str, Bounds<Pixels>>>> =
        const { RefCell::new(None) };
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

/// Start recording where named regions are drawn; elements built after this
/// are wrapped, and each draw replaces a region's bounds.
#[cfg(test)]
pub(crate) fn record_regions() {
    REGIONS.with(|regions| *regions.borrow_mut() = Some(HashMap::new()));
}

/// Where `name` was last drawn, in window pixels.
#[cfg(test)]
pub(crate) fn region_bounds(name: &str) -> Option<Bounds<Pixels>> {
    REGIONS.with(|regions| regions.borrow().as_ref()?.get(name).copied())
}

/// The names recorded so far, sorted.
#[cfg(test)]
pub(crate) fn region_names() -> Vec<&'static str> {
    REGIONS.with(|regions| {
        let mut names: Vec<_> = regions
            .borrow()
            .as_ref()
            .map(|map| map.keys().copied().collect())
            .unwrap_or_default();
        names.sort_unstable();
        names
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
