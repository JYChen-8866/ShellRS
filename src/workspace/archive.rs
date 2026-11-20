//! 文件's system archive commands. They run beside the terminal's shell,
//! where the user is already logged in, and the pane refreshes afterwards.

use std::time::Duration;

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;

use crate::app::ExplorerCommand;
use crate::explorer::{ExplorerId, ExplorerPanel, archive};
use crate::i18n::t;
use crate::terminal::exec_answer;

use super::Workspace;

const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

enum ArchiveRun {
    Compress { names: Vec<String>, archive: String },
    Extract { archive: String },
}

impl Workspace {
    /// Run the workspace-level archive commands. Returns whether `command`
    /// was one of them; the explorer panel handles everything else.
    pub(super) fn handle_archive_command(
        &mut self,
        id: ExplorerId,
        panel: Entity<ExplorerPanel>,
        command: &ExplorerCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let run = match command {
            ExplorerCommand::Archive {
                remote: true,
                names,
                archive,
            } => Some(ArchiveRun::Compress {
                names: names.clone(),
                archive: archive.clone(),
            }),
            ExplorerCommand::Extract {
                remote: true,
                names,
            } if names.len() == 1 => Some(ArchiveRun::Extract {
                archive: names[0].clone(),
            }),
            ExplorerCommand::Archive { .. } | ExplorerCommand::Extract { .. } => {
                self.archive_not_connected(window, cx);
                return true;
            }
            _ => None,
        };
        let Some(run) = run else {
            return false;
        };
        if !panel.read(cx).is_sidebar() {
            self.archive_not_connected(window, cx);
            return true;
        }
        let Some(terminal) = self.sidebar_terminal(id) else {
            self.archive_not_connected(window, cx);
            return true;
        };
        let Some(view) = self
            .terminals
            .get(&terminal)
            .map(|panel| panel.read(cx).terminal().clone())
        else {
            self.archive_not_connected(window, cx);
            return true;
        };
        let directory = panel.read(cx).remote().read(cx).path();
        let (command_line, title, success) = match &run {
            ArchiveRun::Compress { names, archive } => (
                archive::archive_command(&directory, archive, names),
                t!("explorer.archive.compress_failed"),
                t!("explorer.archive.compress_succeeded", name = archive),
            ),
            ArchiveRun::Extract { archive } => (
                archive::extract_command(&directory, archive),
                t!("explorer.archive.extract_failed"),
                t!("explorer.archive.extract_succeeded", name = archive),
            ),
        };
        let Some(command_line) = command_line else {
            window.push_notification(
                Notification::error(t!("explorer.archive.command_failed")).title(title),
                cx,
            );
            return true;
        };
        let Some(reply) = view
            .read(cx)
            .exec_with_timeout(command_line, ARCHIVE_TIMEOUT, cx)
        else {
            self.archive_not_connected(window, cx);
            return true;
        };
        let remote_pane = panel.read(cx).remote().clone();
        remote_pane.update(cx, |pane, cx| pane.set_busy(true, cx));
        cx.spawn_in(window, async move |this, cx| {
            let outcome = match exec_answer(reply, cx).await {
                None => Err(t!("explorer.archive.not_connected").to_string()),
                Some(Err(error)) => Err(error),
                Some(Ok(output)) if archive::command_succeeded(&output) => Ok(()),
                Some(Ok(output)) => Err(archive::command_output(&output)
                    .unwrap_or_else(|| t!("explorer.archive.command_failed").to_string())),
            };
            let _ = this.update_in(cx, |_, window, cx| {
                let notification = match outcome {
                    Ok(()) => Notification::success(success),
                    Err(error) => Notification::error(error).title(title),
                };
                panel.update(cx, |panel, cx| {
                    panel
                        .remote()
                        .update(cx, |pane, cx| pane.set_busy(false, cx));
                    panel.reload(true, window, cx);
                });
                window.push_notification(notification, cx);
            });
        })
        .detach();
        true
    }

    fn archive_not_connected(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(
            Notification::error(t!("explorer.archive.not_connected")),
            cx,
        );
    }
}
