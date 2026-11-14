//! A window-level image lightbox: a dimmed backdrop with one fitted image.
//!
//! The root plugin keeps the overlay above the application and its dialogs.
//! The image alone is the interactive frame; pressing anywhere else closes it.

use std::{sync::Arc, time::Duration};

use gpui_kit::component::{ActiveTheme as _, Sizable as _, spinner::Spinner};
use gpui_kit::*;

use crate::i18n::t;

const MAX_VIEWPORT_SHARE: f32 = 0.72;
const FADE_DURATION: Duration = Duration::from_millis(220);
const FRAME_ENTER_DURATION: Duration = Duration::from_millis(280);

#[derive(Clone)]
struct ActiveImagePreview(WeakEntity<ImagePreviewLayer>);
impl Global for ActiveImagePreview {}

/// The per-window root plugin that renders the image lightbox.
pub(crate) struct ImagePreviewLayer {
    focus_handle: FocusHandle,
    current: Option<ActivePreview>,
    generation: u64,
    _decode: Task<()>,
    _close: Task<()>,
}

struct ActivePreview {
    name: SharedString,
    decoded: Option<Result<Arc<RenderImage>, String>>,
    closing: bool,
    previous_focus: Option<FocusHandle>,
}

impl ImagePreviewLayer {
    fn new(cx: &mut Context<Self>) -> Self {
        let layer = Self {
            focus_handle: cx.focus_handle(),
            current: None,
            generation: 0,
            _decode: Task::ready(()),
            _close: Task::ready(()),
        };
        cx.set_global(ActiveImagePreview(cx.entity().downgrade()));
        cx.on_release(|this, cx| {
            if let Some(ActivePreview {
                decoded: Some(Ok(image)),
                ..
            }) = this.current.take()
            {
                cx.defer(move |cx| cx.drop_image(image, None));
            }
        })
        .detach();
        layer
    }

    fn show(
        &mut self,
        image: Image,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let previous_focus = window.focused(cx);
        self.current = Some(ActivePreview {
            name,
            decoded: None,
            closing: false,
            previous_focus,
        });
        let renderer = cx.svg_renderer();
        self._decode = cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_spawn(async move { image.to_image_data(renderer) })
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation
                    && let Some(preview) = this.current.as_mut()
                {
                    preview.decoded = Some(decoded);
                    cx.notify();
                }
            });
        });
        let focus = self.focus_handle.clone();
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.current.as_mut() else {
            return;
        };
        if preview.closing {
            return;
        }
        preview.closing = true;
        if let Some(focus) = preview.previous_focus.clone() {
            window.focus(&focus, cx);
        }
        let generation = self.generation;
        cx.notify();
        self._close = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FADE_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation
                    || !this.current.as_ref().is_some_and(|preview| preview.closing)
                {
                    return;
                }
                if let Some(preview) = this.current.take()
                    && let Some(Ok(image)) = preview.decoded
                {
                    cx.defer(move |cx| cx.drop_image(image, None));
                }
                cx.notify();
            });
        });
    }

    fn close_on_escape(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
            self.close(window, cx);
            cx.stop_propagation();
        }
    }
}

impl gpui_kit::base::RootPlugin for ImagePreviewLayer {}

impl Render for ImagePreviewLayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(preview) = self.current.as_ref() else {
            return div().into_any_element();
        };
        let closing = preview.closing;
        let name = preview.name.clone();
        let generation = self.generation;
        let content = match &preview.decoded {
            Some(Ok(image)) => {
                let viewport = window.viewport_size();
                let shown = fitted_size(image.size(0), viewport);
                let frame = div()
                    .id("preview-image")
                    .test_support()
                    .aria_label(name.clone())
                    .w(shown.width)
                    .h(shown.height)
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().foreground.opacity(0.22))
                    .overflow_hidden()
                    .shadow(image_frame_shadow())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        img(ImageSource::Render(image.clone()))
                            .id("preview-picture")
                            .size_full()
                            .object_fit(ObjectFit::Contain)
                            .test_support(),
                    );
                frame
                    .with_animation(
                        ElementId::Name(format!("image-preview-frame-{generation}").into()),
                        Animation::new(FRAME_ENTER_DURATION).with_easing(ease_out_quint()),
                        |frame, delta| frame.opacity(0.72 + delta * 0.28),
                    )
                    .into_any_element()
            }
            Some(Err(error)) => div()
                .id("preview-note")
                .test_support()
                .aria_label(t!("shared.image_preview.undecodable"))
                .max_w(rems(24.))
                .rounded(cx.theme().radius_lg)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .px_4()
                .py_3()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(SharedString::from(error.clone()))
                .into_any_element(),
            None => div()
                .id("preview-loading")
                .test_support()
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new().large().color(cx.theme().foreground))
                .into_any_element(),
        };
        let overlay = div()
            .id("image-preview-overlay")
            .test_support()
            .aria_label(name)
            .role(Role::Dialog)
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(black().opacity(0.55))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::close_on_escape))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close(window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(content)
            .with_animation(
                if closing {
                    format!("image-preview-overlay-out-{generation}")
                } else {
                    format!("image-preview-overlay-in-{generation}")
                },
                Animation::new(FADE_DURATION).with_easing(ease_out_quint()),
                move |overlay, delta| {
                    let opacity = if closing { 1. - delta } else { delta };
                    overlay.opacity(opacity)
                },
            );
        overlay.into_any_element()
    }
}

fn fitted_size(image: Size<DevicePixels>, viewport: Size<Pixels>) -> Size<Pixels> {
    let width = i32::from(image.width).max(1) as f32;
    let height = i32::from(image.height).max(1) as f32;
    let scale = ((viewport.width * MAX_VIEWPORT_SHARE).as_f32() / width)
        .min((viewport.height * MAX_VIEWPORT_SHARE).as_f32() / height);
    size(px(width * scale), px(height * scale))
}

fn image_frame_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: black().opacity(0.48),
            offset: point(px(0.), px(28.)),
            blur_radius: px(64.),
            spread_radius: px(4.),
            inset: false,
        },
        BoxShadow {
            color: black().opacity(0.34),
            offset: point(px(0.), px(10.)),
            blur_radius: px(24.),
            spread_radius: px(-2.),
            inset: false,
        },
        BoxShadow {
            color: white().opacity(0.18),
            offset: point(px(0.), px(-2.)),
            blur_radius: px(8.),
            spread_radius: px(-4.),
            inset: false,
        },
    ]
}

pub(crate) fn open_image_preview(
    image: Image,
    name: SharedString,
    _bytes: Option<u64>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(layer) = cx
        .try_global::<ActiveImagePreview>()
        .and_then(|active| active.0.upgrade())
    else {
        return;
    };
    layer.update(cx, |layer, cx| layer.show(image, name, window, cx));
}

pub(crate) fn build_image_preview_layer(
    _window: &mut Window,
    cx: &mut Context<ImagePreviewLayer>,
) -> ImagePreviewLayer {
    ImagePreviewLayer::new(cx)
}

#[cfg(test)]
mod tests {
    use super::{fitted_size, image_frame_shadow};
    use gpui_kit::{DevicePixels, px, size};

    #[test]
    fn fitted_images_keep_their_aspect_ratio_inside_the_viewport() {
        let wide = fitted_size(
            size(DevicePixels::from(400), DevicePixels::from(200)),
            size(px(1000.), px(800.)),
        );
        assert_eq!(wide.width, px(720.));
        assert_eq!(wide.height, px(360.));

        let tall = fitted_size(
            size(DevicePixels::from(200), DevicePixels::from(400)),
            size(px(1000.), px(800.)),
        );
        assert_eq!(tall.width, px(288.));
        assert_eq!(tall.height, px(576.));
    }

    #[test]
    fn the_image_frame_has_three_depth_shadows() {
        let shadows = image_frame_shadow();
        assert_eq!(shadows.len(), 3);
        assert_eq!(shadows[0].offset.y, px(28.));
        assert_eq!(shadows[1].offset.y, px(10.));
        assert_eq!(shadows[2].offset.y, px(-2.));
    }
}
