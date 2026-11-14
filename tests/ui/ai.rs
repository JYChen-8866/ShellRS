//! The right sidebar's AI assistant panel.

use crate::support::*;
use base64::Engine as _;
use gpui_kit::{Image, ImageFormat};
use shellrs::app::CopyAiImage;

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
        assert!(window.find("ai-context-progress").visible());
        let send = window.find("ai-send").bounds();
        assert_eq!(send.size, size(px(28.), px(28.)));
        let control = window.find("ai-context-progress").bounds();
        assert_eq!(control.size, size(px(36.), px(36.)));
        let composer = window.find("ai-composer").bounds();
        for id in ["ai-attach", "ai-confirmation", "ai-model", "ai-send"] {
            let control = window.find(id).bounds();
            assert!(
                control.left() >= composer.left()
                    && control.right() <= composer.right()
                    && control.top() >= composer.top()
                    && control.bottom() <= composer.bottom(),
                "{id} should float inside the composer: {control:?} {composer:?}"
            );
        }
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
fn copying_a_sent_image_writes_an_image_to_the_clipboard(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sent-image.png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    std::fs::write(&path, bytes).unwrap();

    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_action(Box::new(CopyAiImage(path.clone())), cx);
    })
    .unwrap();
    cx.run_until_parked();

    let copied = cx.update(|cx| cx.read_from_clipboard()).unwrap();
    assert!(
        copied
            .entries()
            .iter()
            .any(|entry| { matches!(entry, gpui_kit::ClipboardEntry::Image(_)) })
    );
}

#[gpui_kit::test]
async fn pasted_image_appears_as_a_thumbnail_attachment(cx: &mut TestAppContext) {
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
        window.click("ai-attachment-image-0", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("image-preview-overlay").visible());
        assert!(window.find("preview-image").visible());
        window.press("escape", cx);
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(1), |window, cx| {
        window.render_frame(cx);
        window.try_find("image-preview-overlay").is_none()
    })
    .await;
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("ai-attachment-remove-0", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("image-preview-overlay").is_none());
        assert!(window.try_find("ai-attachment-0").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn shift_enter_at_the_last_line_grows_the_composer_immediately(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tool-ai", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("ai-composer", cx);
        window.input("one\ntwo\nthree", cx);
    })
    .unwrap();
    cx.run_until_parked();

    let before = cx
        .update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.find("ai-composer").bounds().size.height
        })
        .unwrap();

    cx.update_window(handle.into(), |_, window, cx| {
        window.press("shift-enter", cx)
    })
    .unwrap();
    cx.run_until_parked();

    let after = cx
        .update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.find("ai-composer").bounds().size.height
        })
        .unwrap();
    assert!(
        after > before,
        "Shift+Enter on the last line should grow the composer immediately: {before:?} -> {after:?}"
    );
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
        let controls_top = ["ai-attach", "ai-confirmation", "ai-model", "ai-send"]
            .into_iter()
            .filter_map(|id| window.try_find(id).map(|control| control.bounds().top()))
            .min()
            .expect("the composer controls should be visible");
        assert!(
            placeholder.bounds().bottom() <= controls_top,
            "the placeholder should stay above the floating controls: {:?} {controls_top:?}",
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
