use crate::{
    AnyElement, AnyEntity, AnyWeakEntity, App, Bounds, ContentMask, Context, Element, ElementId,
    Entity, EntityId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    OffscreenSurfaceId, PaintIndex, Pixels, PrepaintStateIndex, Render, Style, StyleRefinement,
    TextStyle, WeakEntity,
};
use crate::{Empty, Window};
use anyhow::Result;
use collections::FxHashSet;
use refineable::Refineable;
use std::mem;
use std::rc::Rc;
use std::{any::TypeId, fmt, ops::Range};

struct AnyViewState {
    prepaint_range: Range<PrepaintStateIndex>,
    paint_range: Range<PaintIndex>,
    cache_key: ViewCacheKey,
    accessed_entities: FxHashSet<EntityId>,
}

#[derive(Default)]
struct ViewCacheKey {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
    offscreen_surface: Option<OffscreenSurfaceId>,
}

/// A dynamically-typed handle to a view, which can be downcast to a [Entity] for a specific type.
#[derive(Clone, Debug)]
pub struct AnyView {
    entity: AnyEntity,
    render: fn(&AnyView, &mut Window, &mut App) -> AnyElement,
    cached_style: Option<Rc<StyleRefinement>>,
    offscreen_surface: Option<OffscreenSurfaceId>,
}

impl<V: Render> From<Entity<V>> for AnyView {
    fn from(value: Entity<V>) -> Self {
        AnyView {
            entity: value.into_any(),
            render: any_view::render::<V>,
            cached_style: None,
            offscreen_surface: None,
        }
    }
}

impl AnyView {
    /// Indicate that this view should be cached when using it as an element.
    /// When using this method, the view's previous layout and paint will be recycled from the previous frame if [Context::notify] has not been called since it was rendered.
    /// The one exception is when [Window::refresh] is called, in which case caching is ignored.
    pub fn cached(mut self, style: StyleRefinement) -> Self {
        self.cached_style = Some(style.into());
        self.offscreen_surface = None;
        self
    }

    /// Cache this view's layout and pixels in a viewport-sized GPU surface.
    /// Unchanged frames composite the surface while retaining hitboxes and input
    /// handlers. Invalidation follows [`Self::cached`]. The style must provide
    /// the viewport size and the surface id must be unique within the window.
    pub fn cached_offscreen(mut self, style: StyleRefinement, id: OffscreenSurfaceId) -> Self {
        self.cached_style = Some(style.into());
        self.offscreen_surface = Some(id);
        self
    }

    /// Convert this to a weak handle.
    pub fn downgrade(&self) -> AnyWeakView {
        AnyWeakView {
            entity: self.entity.downgrade(),
            render: self.render,
        }
    }

    /// Convert this to a [Entity] of a specific type.
    /// If this handle does not contain a view of the specified type, returns itself in an `Err` variant.
    pub fn downcast<T: 'static>(self) -> Result<Entity<T>, Self> {
        match self.entity.downcast() {
            Ok(entity) => Ok(entity),
            Err(entity) => Err(Self {
                entity,
                render: self.render,
                cached_style: self.cached_style,
                offscreen_surface: self.offscreen_surface,
            }),
        }
    }

    /// Gets the [TypeId] of the underlying view.
    pub fn entity_type(&self) -> TypeId {
        self.entity.entity_type
    }

    /// Gets the entity id of this handle.
    pub fn entity_id(&self) -> EntityId {
        self.entity.entity_id()
    }
}

impl PartialEq for AnyView {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
    }
}

impl Eq for AnyView {}

impl Element for AnyView {
    type RequestLayoutState = Option<AnyElement>;
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::View(self.entity_id()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        window.with_rendered_view(self.entity_id(), |window| {
            // Disable caching when inspecting so that mouse_hit_test has all hitboxes.
            let caching_disabled = window.is_inspector_picking(cx);
            match self.cached_style.as_ref() {
                Some(style) if !caching_disabled => {
                    let mut root_style = Style::default();
                    root_style.refine(style);
                    let layout_id = window.request_layout(root_style, None, cx);
                    (layout_id, None)
                }
                _ => {
                    let mut element = (self.render)(self, window, cx);
                    let layout_id = element.request_layout(window, cx);
                    (layout_id, Some(element))
                }
            }
        })
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        window.set_view_id(self.entity_id());
        window.with_rendered_view(self.entity_id(), |window| {
            if let Some(mut element) = element.take() {
                element.prepaint(window, cx);
                return Some(element);
            }

            window.with_element_state::<AnyViewState, _>(
                global_id.unwrap(),
                |element_state, window| {
                    let content_mask = window.content_mask();
                    let text_style = window.text_style();

                    if let Some(mut element_state) = element_state
                        && element_state.cache_key.bounds == bounds
                        && element_state.cache_key.content_mask == content_mask
                        && element_state.cache_key.text_style == text_style
                        && element_state.cache_key.offscreen_surface == self.offscreen_surface
                        && !window.dirty_views.contains(&self.entity_id())
                        && !window.refreshing
                    {
                        let prepaint_start = window.prepaint_index();
                        window.reuse_prepaint(element_state.prepaint_range.clone());
                        cx.entities
                            .extend_accessed(&element_state.accessed_entities);
                        let prepaint_end = window.prepaint_index();
                        element_state.prepaint_range = prepaint_start..prepaint_end;

                        return (None, element_state);
                    }

                    let refreshing = mem::replace(&mut window.refreshing, true);
                    let prepaint_start = window.prepaint_index();
                    let (mut element, accessed_entities) = cx.detect_accessed_entities(|cx| {
                        let mut element = (self.render)(self, window, cx);
                        element.layout_as_root(bounds.size.into(), window, cx);
                        element.prepaint_at(bounds.origin, window, cx);
                        element
                    });

                    let prepaint_end = window.prepaint_index();
                    window.refreshing = refreshing;

                    (
                        Some(element),
                        AnyViewState {
                            accessed_entities,
                            prepaint_range: prepaint_start..prepaint_end,
                            paint_range: PaintIndex::default()..PaintIndex::default(),
                            cache_key: ViewCacheKey {
                                bounds,
                                content_mask,
                                text_style,
                                offscreen_surface: self.offscreen_surface,
                            },
                        },
                    )
                },
            )
        })
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        element: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_rendered_view(self.entity_id(), |window| {
            let caching_disabled = window.is_inspector_picking(cx);
            if self.cached_style.is_some() && !caching_disabled {
                window.with_element_state::<AnyViewState, _>(
                    global_id.unwrap(),
                    |element_state, window| {
                        let mut element_state = element_state.unwrap();

                        let paint_start = window.paint_index();

                        if let Some(element) = element {
                            let refreshing = mem::replace(&mut window.refreshing, true);
                            if let Some(id) = self.offscreen_surface {
                                window.paint_offscreen(id, bounds, true, |window| {
                                    element.paint(window, cx);
                                });
                            } else {
                                element.paint(window, cx);
                            }
                            window.refreshing = refreshing;
                        } else if let Some(id) = self.offscreen_surface {
                            // Retain event handlers and element state without replaying the
                            // captured drawing commands into either the frame or the texture.
                            window.reuse_paint_without_scene(element_state.paint_range.clone());
                            window.paint_offscreen(id, bounds, false, |_| {});
                        } else {
                            window.reuse_paint(element_state.paint_range.clone());
                        }

                        let paint_end = window.paint_index();
                        element_state.paint_range = paint_start..paint_end;

                        ((), element_state)
                    },
                )
            } else {
                element.as_mut().unwrap().paint(window, cx);
            }
        });
    }
}

impl<V: 'static + Render> IntoElement for Entity<V> {
    type Element = AnyView;

    fn into_element(self) -> Self::Element {
        self.into()
    }
}

impl IntoElement for AnyView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A weak, dynamically-typed view handle that does not prevent the view from being released.
pub struct AnyWeakView {
    entity: AnyWeakEntity,
    render: fn(&AnyView, &mut Window, &mut App) -> AnyElement,
}

impl AnyWeakView {
    /// Convert to a strongly-typed handle if the referenced view has not yet been released.
    pub fn upgrade(&self) -> Option<AnyView> {
        let entity = self.entity.upgrade()?;
        Some(AnyView {
            entity,
            render: self.render,
            cached_style: None,
            offscreen_surface: None,
        })
    }
}

impl<V: 'static + Render> From<WeakEntity<V>> for AnyWeakView {
    fn from(view: WeakEntity<V>) -> Self {
        AnyWeakView {
            entity: view.into(),
            render: any_view::render::<V>,
        }
    }
}

impl PartialEq for AnyWeakView {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
    }
}

impl std::fmt::Debug for AnyWeakView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyWeakView")
            .field("entity_id", &self.entity.entity_id)
            .finish_non_exhaustive()
    }
}

mod any_view {
    use crate::{AnyElement, AnyView, App, IntoElement, Render, Window};

    pub(crate) fn render<V: 'static + Render>(
        view: &AnyView,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let view = view.clone().downcast::<V>().unwrap();
        view.update(cx, |view, cx| view.render(window, cx).into_any_element())
    }
}

/// A view that renders nothing
pub struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppContext, InteractiveElement, MouseButton, ParentElement, Styled, TestAppContext,
        VisualTestContext, div, px, rgb,
    };

    struct SurfaceContent {
        clicks: usize,
    }

    impl Render for SurfaceContent {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(rgb(0x123456))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, _, cx| {
                        view.clicks += 1;
                        cx.notify();
                    }),
                )
                .child(format!("clicks: {}", self.clicks))
        }
    }

    struct SurfaceRoot {
        content: Entity<SurfaceContent>,
        width: f32,
    }

    const SURFACE_ID: OffscreenSurfaceId = OffscreenSurfaceId(123);

    impl Render for SurfaceRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).h(px(200.)).child(
                AnyView::from(self.content.clone())
                    .cached_offscreen(StyleRefinement::default().size_full(), SURFACE_ID),
            )
        }
    }

    fn redraw(cx: &mut VisualTestContext, update: impl FnOnce(&mut App)) -> bool {
        cx.update(|window, cx| {
            // Apply notifications inside the same App update as the explicit draw;
            // otherwise App's effect flush may draw before we inspect the result.
            update(cx);
            let _ = window.draw(cx);
            let scene = &window.rendered_frame.scene;
            let surface = scene
                .offscreen_surfaces
                .iter()
                .find(|s| s.id == SURFACE_ID)
                .unwrap();
            assert!(
                scene.quads.is_empty(),
                "content belongs in the texture, not the main scene"
            );
            surface.scene.is_some()
        })
    }

    #[crate::test]
    fn cached_offscreen_reuses_pixels_and_retains_input(cx: &mut TestAppContext) {
        let (root, cx) = cx.add_window_view(|_, cx| SurfaceRoot {
            content: cx.new(|_| SurfaceContent { clicks: 0 }),
            width: 300.,
        });
        cx.run_until_parked();
        for _ in 0..5 {
            assert!(
                !redraw(cx, |cx| root.update(cx, |_, cx| cx.notify())),
                "unchanged frames only composite the texture"
            );
        }
        let content = root.read_with(cx, |root, _| root.content.clone());
        cx.simulate_mouse_down(
            crate::point(px(20.), px(20.)),
            MouseButton::Left,
            Default::default(),
        );
        assert_eq!(content.read_with(cx, |content, _| content.clicks), 1);
        assert!(
            redraw(cx, |cx| content.update(cx, |content, cx| {
                content.clicks += 1;
                cx.notify();
            })),
            "descendant notifications repaint the surface"
        );
        assert!(!redraw(cx, |cx| root.update(cx, |_, cx| cx.notify())));
        root.update(cx, |root, _| root.width = 400.);
        assert!(
            redraw(cx, |_| {}),
            "resizing reallocates and repaints the surface"
        );
        assert!(!redraw(cx, |cx| root.update(cx, |_, cx| cx.notify())));
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
            assert!(
                window
                    .rendered_frame
                    .scene
                    .offscreen_surfaces
                    .iter()
                    .any(|surface| surface.id == SURFACE_ID && surface.scene.is_some()),
                "full refresh repaints the surface"
            );
        });
    }
}
