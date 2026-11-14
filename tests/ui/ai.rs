//! The right sidebar's AI assistant panel.

use crate::support::*;
use base64::Engine as _;
use gpui_kit::{Image, ImageFormat};

#[gpui_kit::test]
fn the_ai_tool_opens_in_the_right_sidebar_for_the_remote_terminal(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("tool-ai").visible());
        window.click("tool-ai", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("tool-sidebar", INITIAL_WEB_TERMINAL)).label(),
            Some("AI 助手")
        );
        assert!(window.find("ai-assistant").visible());
        assert!(window.find("ai-empty").visible());
        assert!(window.find("ai-history").visible());
        assert!(window.find("ai-new-chat").visible());
        assert!(window.find("ai-attach").visible());
        assert!(window.find("ai-confirmation").visible());
        assert!(window.find("ai-model").visible());
        assert!(window.find("ai-send").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_history_button_opens_the_conversation_dialog(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tool-ai", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("ai-history", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("ai-history-search").visible());
        assert_eq!(window.find("ai-history-search").focused(), Some(true));
        assert!(window.find("ai-history-clear").visible());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("ai-history-search").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn pasted_image_appears_as_a_thumbnail_attachment(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tool-ai", cx);
    })
    .unwrap();
    cx.run_until_parked();

    let bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let image = Image::from_bytes(ImageFormat::Png, bytes);
    cx.update(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_image(&image));
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("ai-composer", cx);
        window.press("cmd-v", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("ai-attachment-0").visible());
        window.click("ai-attachment-remove-0", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("ai-attachment-0").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_composer_placeholder_wraps_with_the_panel_width(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tool-ai", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.dock_area().update(cx, |area, cx| {
                area.set_dock_size(
                    gpui_kit::component::dock::DockPlacement::Right,
                    px(220.),
                    window,
                    cx,
                );
            });
        });
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let placeholder = window.find("ai-composer-placeholder");
        assert!(placeholder.visible());
        assert!(
            placeholder.bounds().size.height > px(24.),
            "the placeholder should wrap past one line: {:?}",
            placeholder.bounds()
        );
        window.click("ai-composer", cx);
        window.input("hello", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("ai-composer-placeholder").is_none());
    })
    .unwrap();
}
