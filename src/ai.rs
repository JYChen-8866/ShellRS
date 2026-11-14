//! 聊天: the AI assistant in the right sidebar. Like the other tools there,
//! the panel stays alive while hidden and is told which SSH terminal is in
//! front. The assistant service itself is not wired up yet, so a sent
//! message stays in this session and says so.

mod history;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use uuid::Uuid;

use base64::Engine as _;
use gpui_kit::base::input::Copy as CopySelection;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, ThemeStyled as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    diff::{Diff, DiffFile, DiffState},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    list::{List, ListDelegate, ListItem, ListState},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    notification::Notification,
    questionnaire::{
        Questionnaire, QuestionnaireActions, QuestionnaireChoice, QuestionnaireChoiceDefinition,
        QuestionnaireChoiceDescription, QuestionnaireChoices, QuestionnaireDescription,
        QuestionnaireError, QuestionnaireEvent, QuestionnaireInput, QuestionnaireInputDefinition,
        QuestionnaireItem, QuestionnaireItemDefinition, QuestionnaireNext, QuestionnairePrevious,
        QuestionnaireProgress, QuestionnaireSkip, QuestionnaireState, QuestionnaireSubmit,
        QuestionnaireTitle,
    },
    scroll::Scrollbar,
    shimmer::ShimmerText,
    spinner::Spinner,
    tag::Tag,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{CatalogIcon, CopyAiImage};
use crate::claude::{self, ClaudeEvent};
use crate::i18n::t;
use crate::md;
use crate::settings::{
    AiAssistantSettings, AiConfirmationStrategy, AiExecutionMode, Choice as _, SettingsStore,
};
use crate::shared::open_image_preview;
use crate::terminal::TerminalFont;
use claude_codes::{ClaudeModel, MediaType};

/// Keep provider chunks off the UI thread until a full presentation frame is
/// due. Waku's streaming work showed that one commit per token can cost far
/// more than the parsing itself; ~8 Hz keeps output responsive while bounding
/// repaint, layout and markdown work.
const AI_STREAM_COMMIT_INTERVAL: Duration = Duration::from_millis(120);
/// Width that fits the queued-message actions: 编辑 / 调整方向 / 关闭 / 复制.
const QUEUED_MESSAGE_ACTIONS_MIN_WIDTH: f32 = 10.5;

fn is_send_shortcut(keystroke: &Keystroke) -> bool {
    keystroke.key == "enter"
        && keystroke.modifiers.control
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.shift
        && !keystroke.modifiers.platform
        && !keystroke.modifiers.function
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Author {
    User,
    Assistant,
    Tool,
    Notice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ToolKind {
    Command,
    Read,
    Edit,
    Search,
    Fetch,
    Other,
}

#[derive(Clone, Debug)]
struct ToolFileChange {
    path: String,
    additions: usize,
    deletions: usize,
    patch: String,
}

#[derive(Clone)]
struct ToolCall {
    id: String,
    name: String,
    summary: SharedString,
    input_text: SharedString,
    output: Option<SharedString>,
    status: ToolStatus,
    file_change: Option<ToolFileChange>,
    diff_state: Option<Entity<DiffState>>,
}

#[derive(Clone, Debug)]
struct PendingPermission {
    request_id: String,
    tool_name: String,
    input: serde_json::Value,
    reason: Option<String>,
}

#[derive(Clone, Debug)]
struct Skill {
    slug: String,
    name: String,
    description: Option<String>,
    category: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkillTrigger {
    range: Range<usize>,
    query: String,
}

#[derive(Clone)]
struct Message {
    author: Author,
    text: String,
    time: Option<SharedString>,
    images: Vec<PathBuf>,
    queue_position: Option<usize>,
    thinking_tail: SharedString,
    markdown_view: Option<Rc<RefCell<md::render::MarkdownView>>>,
    tools: Vec<ToolCall>,
}

struct ClaudeBridge {
    prompts: async_channel::Sender<claude::ClaudePrompt>,
    commands: async_channel::Sender<claude::ClaudeCommand>,
    permissions: async_channel::Sender<claude::PermissionDecision>,
    _worker: std::thread::JoinHandle<()>,
    _events: Task<()>,
}

/// A retained animation island. The message list can update streaming text at
/// its own cadence without rebuilding the spinner/shimmer row on every display
/// frame. Zed uses the same `window.use_state` pattern for `GeneratingSpinner`.
struct ThinkingSpinner;

impl Render for ThinkingSpinner {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_2()
            .child(Spinner::new().small().color(cx.theme().primary))
            .child(
                ShimmerText::new(t!("ai.message.thinking"))
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().primary)
                    .spread(0.35),
            )
    }
}

/// 聊天: one conversation inside the right sidebar. `ToolSidebar` points it
/// at the terminal in front only while this is the tool on screen, as it
/// does for 历史命令 and the other tools.
pub struct AiAssistantPanel {
    settings: Entity<SettingsStore>,
    host: Option<SharedString>,
    input: Entity<TextareaState>,
    messages: Rc<Vec<Message>>,
    claude: Option<ClaudeBridge>,
    session_id: Option<Uuid>,
    resume_session: bool,
    claude_status: Option<(SharedString, Hsla)>,
    context_usage: Option<claude::ClaudeContextUsage>,
    streaming: Option<usize>,
    busy: bool,
    pending_turns: usize,
    queued_prompts: VecDeque<claude::ClaudePrompt>,
    stopping: bool,
    background: bool,
    confirmation: AiConfirmationStrategy,
    permission: Option<PendingPermission>,
    permission_state: Option<Entity<QuestionnaireState>>,
    permission_subscription: Option<Subscription>,
    expanded_tools: HashSet<String>,
    expanded_tool_groups: HashMap<String, bool>,
    tool_scrolls: HashMap<String, ScrollHandle>,
    attachments: Vec<PathBuf>,
    model: Option<ClaudeModel>,
    skills: Vec<Skill>,
    skill_menu_index: usize,
    skill_menu_dismissed: bool,
    skill_menu_scroll: ScrollHandle,
    skill_trigger: Option<SkillTrigger>,
    skill_menu_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    message_scroller: Entity<MessageScrollerState>,
    markdown_selection: md::render::TranscriptSelection,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AiAssistantPanel {
    pub fn new(
        settings: Entity<SettingsStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ai = settings.read(cx).settings().ai;
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .submit_on_enter(false)
                .soft_wrap(true)
                .auto_grow(3, 7)
        });
        let input_focus = input.read(cx).focus_handle(cx);
        let skill_panel = cx.weak_entity();
        let skill_keys = cx.intercept_keystrokes(move |event, window, cx| {
            if !input_focus.is_focused(window) {
                return;
            }
            if is_send_shortcut(&event.keystroke) {
                let _ = skill_panel.update(cx, |panel, cx| panel.send(window, cx));
                cx.stop_propagation();
                return;
            }
            let keystroke = event.keystroke.clone();
            let handled = skill_panel
                .update(cx, |panel, cx| {
                    panel.handle_skill_key(&keystroke, window, cx)
                })
                .unwrap_or(false);
            if handled {
                cx.stop_propagation();
            }
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.skill_menu_index = 0;
                        this.skill_menu_dismissed = false;
                        this.skill_menu_scroll.scroll_to_item(0);
                        cx.notify();
                    }
                },
            ),
            cx.observe_in(&settings, window, |this, settings, _window, cx| {
                let confirmation = settings.read(cx).settings().ai.confirmation;
                if confirmation != this.confirmation {
                    this.confirmation = confirmation;
                    this.reset_session(cx);
                }
                let scroller = this.message_scroller.clone();
                scroller.update(cx, |state, cx| state.remeasure(cx));
                cx.notify();
            }),
            cx.observe_global_in::<crate::i18n::UiLocale>(window, |_this, _window, cx| {
                cx.notify();
            }),
            skill_keys,
        ];
        Self {
            settings,
            host: None,
            input,
            messages: Rc::new(Vec::new()),
            claude: None,
            session_id: None,
            resume_session: false,
            claude_status: None,
            context_usage: None,
            streaming: None,
            busy: false,
            pending_turns: 0,
            queued_prompts: VecDeque::new(),
            stopping: false,
            background: ai.execution == AiExecutionMode::Background,
            confirmation: ai.confirmation,
            permission: None,
            permission_state: None,
            permission_subscription: None,
            expanded_tools: HashSet::new(),
            expanded_tool_groups: HashMap::new(),
            tool_scrolls: HashMap::new(),
            attachments: Vec::new(),
            model: None,
            skills: discover_skills(&claude::working_directory(ai.working_directory)),
            skill_menu_index: 0,
            skill_menu_dismissed: false,
            skill_menu_scroll: ScrollHandle::new(),
            skill_trigger: None,
            skill_menu_bounds: Rc::new(Cell::new(None)),
            message_scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            markdown_selection: md::render::TranscriptSelection::default(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn messages_mut(&mut self) -> &mut Vec<Message> {
        Rc::make_mut(&mut self.messages)
    }

    fn input_source(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    fn set_input_source(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }

    fn focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Point the assistant at `host` while `active`, the panel showing it.
    /// Switching tools or terminals only updates the host used by the next
    /// prompt; the conversation and its Claude worker stay alive until the
    /// user explicitly starts a new conversation.
    pub fn set_target(&mut self, host: Option<SharedString>, active: bool, cx: &mut Context<Self>) {
        let changed = self.host != host;
        if changed {
            self.host = host;
            if self.host.is_some() {
                let ai = self.settings.read(cx).settings().ai;
                self.background = ai.execution == AiExecutionMode::Background;
                self.confirmation = ai.confirmation;
            }
        }
        if changed || active {
            cx.notify();
        }
    }

    fn ai(&self, cx: &App) -> AiAssistantSettings {
        self.settings.read(cx).settings().ai
    }

    fn detect_skill_trigger(&self, cx: &App) -> Option<SkillTrigger> {
        let input = self.input.read(cx);
        let value = input.value();
        let caret = input.selected_range().end;
        let before = value.get(..caret)?;
        let start = before
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                character
                    .is_whitespace()
                    .then_some(index + character.len_utf8())
            })
            .unwrap_or(0);
        let token = &before[start..caret];
        let query = token.strip_prefix('/')?;
        Some(SkillTrigger {
            range: start..caret,
            query: query.to_lowercase(),
        })
    }

    fn sync_skill_trigger(&mut self, cx: &App) {
        let trigger = self.detect_skill_trigger(cx);
        if self.skill_trigger != trigger {
            self.skill_trigger = trigger;
            self.skill_menu_index = 0;
            self.skill_menu_dismissed = false;
            self.skill_menu_scroll.scroll_to_item(0);
        }
    }

    fn filtered_skills(&self, query: &str) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|skill| {
                query.is_empty()
                    || skill.slug.to_lowercase().contains(query)
                    || skill.name.to_lowercase().contains(query)
                    || skill.category.to_lowercase().contains(query)
                    || skill
                        .description
                        .as_deref()
                        .is_some_and(|description| description.to_lowercase().contains(query))
            })
            .collect()
    }

    fn handle_skill_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_skill_trigger(cx);
        if self.skill_menu_dismissed || keystroke.modifiers.modified() {
            return false;
        }
        let Some(trigger) = self.skill_trigger.clone() else {
            return false;
        };
        if self.filtered_skills(&trigger.query).is_empty() {
            return false;
        }
        match keystroke.key.as_str() {
            "up" | "left" => self.move_skill_selection(-1, cx),
            "down" | "right" => self.move_skill_selection(1, cx),
            "enter" | "tab" => self.accept_selected_skill(window, cx),
            "escape" => {
                self.skill_menu_dismissed = true;
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    fn move_skill_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(trigger) = self.skill_trigger.as_ref() else {
            return;
        };
        let len = self.filtered_skills(&trigger.query).len();
        if len == 0 {
            return;
        }
        let current = self.skill_menu_index.min(len - 1);
        let next = (current as isize + delta).rem_euclid(len as isize) as usize;
        self.skill_menu_index = next;
        self.skill_menu_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn accept_selected_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(trigger) = self.skill_trigger.clone() else {
            return;
        };
        let skills = self.filtered_skills(&trigger.query);
        let selected = self.skill_menu_index.min(skills.len().saturating_sub(1));
        let Some(slug) = skills.get(selected).map(|skill| skill.slug.clone()) else {
            return;
        };
        self.insert_skill(&slug, window, cx);
    }

    fn insert_skill(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.skill_menu_index = 0;
        self.skill_menu_dismissed = false;
        self.skill_menu_scroll.scroll_to_item(0);
        let Some(trigger) = self.skill_trigger.clone() else {
            return;
        };
        let value = self.input_source(cx);
        let insert = format!("/{slug} ");
        let end = trigger.range.end.min(value.len());
        let mut text = value[..trigger.range.start].to_string();
        text.push_str(&insert);
        text.push_str(&value[end..]);
        let caret = trigger.range.start + insert.len();
        self.skill_trigger = None;
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.set_selected_range(caret..caret, cx);
        });
        self.focus_input(window, cx);
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host.is_none() {
            return;
        }
        if !self.skill_menu_dismissed
            && let Some(trigger) = self.skill_trigger.as_ref()
            && let Some(slug) = self
                .filtered_skills(&trigger.query)
                .get(self.skill_menu_index)
                .map(|skill| skill.slug.clone())
        {
            self.insert_skill(&slug, window, cx);
            return;
        }
        let text = self.input_source(cx).trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        let starts_turn = self.pending_turns == 0;
        let queue_position = (!starts_turn).then(|| self.queued_prompts.len() + 1);
        self.start_claude(window, cx);
        let Some(prompts) = self.claude.as_ref().map(|claude| claude.prompts.clone()) else {
            self.messages_mut().push(Message {
                author: Author::Notice,
                text: t!("ai.claude.start_failed").to_string(),
                time: None,
                images: Vec::new(),
                queue_position: None,
                markdown_view: None,
                thinking_tail: "".into(),
                tools: Vec::new(),
            });
            cx.notify();
            return;
        };
        let attachments = std::mem::take(&mut self.attachments);
        let mut file_context = String::new();
        let mut images = Vec::new();
        let mut preview_images = Vec::new();
        for path in &attachments {
            if let Some(media_type) = claude_media_type(path) {
                match claude_image_data(path) {
                    Ok(data) => {
                        images.push((media_type, data));
                        preview_images.push(path.clone());
                    }
                    Err(error) => self.messages_mut().push(Message {
                        author: Author::Notice,
                        text: error.to_string(),
                        time: None,
                        images: Vec::new(),
                        queue_position: None,
                        markdown_view: None,
                        thinking_tail: "".into(),
                        tools: Vec::new(),
                    }),
                }
            } else if is_image_path(path) {
                self.messages_mut().push(Message {
                    author: Author::Notice,
                    text: t!(
                        "ai.attachments.image_unsupported",
                        name = path.display().to_string()
                    )
                    .to_string(),
                    time: None,
                    images: Vec::new(),
                    queue_position: None,
                    markdown_view: None,
                    thinking_tail: "".into(),
                    tools: Vec::new(),
                });
            } else {
                match read_attachment(path) {
                    Ok(content) => {
                        file_context.push_str(&format!(
                            "\n\n<file path=\"{}\">\n{content}\n</file>",
                            path.display()
                        ));
                    }
                    Err(error) => self.messages_mut().push(Message {
                        author: Author::Notice,
                        text: error.to_string(),
                        time: None,
                        images: Vec::new(),
                        queue_position: None,
                        markdown_view: None,
                        thinking_tail: "".into(),
                        tools: Vec::new(),
                    }),
                }
            }
        }
        let shown = if text.is_empty() {
            t!("ai.attachments.only").to_string()
        } else {
            text.clone()
        };
        self.set_input_source("", window, cx);
        self.messages_mut().push(Message {
            author: Author::User,
            text: shown,
            time: Some(SharedString::from(
                chrono::Local::now().format("%H:%M").to_string(),
            )),
            images: preview_images,
            queue_position,
            markdown_view: Some(Rc::new(RefCell::new(md::render::MarkdownView::new()))),
            thinking_tail: "".into(),
            tools: Vec::new(),
        });
        let placeholder = starts_turn.then(|| {
            self.messages_mut().push(Message {
                author: Author::Assistant,
                text: "".into(),
                time: None,
                images: Vec::new(),
                queue_position: None,
                markdown_view: Some(Rc::new(RefCell::new(md::render::MarkdownView::seeded()))),
                thinking_tail: "".into(),
                tools: Vec::new(),
            });
            let index = self.messages.len() - 1;
            self.streaming = Some(index);
            index
        });
        let prompt = claude::ClaudePrompt {
            text: match &self.host {
                Some(host) => format!("SSH host: {host}\n\n{text}{file_context}"),
                None => format!("{text}{file_context}"),
            },
            images,
        };
        if starts_turn {
            if prompts.try_send(prompt).is_err() {
                if let Some(index) = placeholder {
                    if index < self.messages.len() {
                        self.messages_mut().remove(index);
                    }
                    self.streaming = None;
                }
                self.messages_mut().push(Message {
                    author: Author::Notice,
                    text: t!("ai.claude.disconnected").to_string(),
                    time: None,
                    images: Vec::new(),
                    queue_position: None,
                    markdown_view: None,
                    thinking_tail: "".into(),
                    tools: Vec::new(),
                });
            } else {
                self.pending_turns += 1;
                self.busy = true;
            }
        } else {
            self.queued_prompts.push_back(prompt);
            self.pending_turns += 1;
            self.busy = true;
        }
        self.sync_message_scroller(cx);
        self.focus_input(window, cx);
        cx.notify();
    }

    fn dispatch_next_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_turns == 0 {
            return;
        }
        let Some(prompt) = self.queued_prompts.pop_front() else {
            return;
        };
        let Some(claude) = &self.claude else {
            self.pending_turns = 0;
            self.clear_queued_messages();
            self.messages_mut().push(Message {
                author: Author::Notice,
                text: t!("ai.claude.disconnected").to_string(),
                time: None,
                images: Vec::new(),
                queue_position: None,
                markdown_view: None,
                thinking_tail: "".into(),
                tools: Vec::new(),
            });
            return;
        };
        if claude.prompts.try_send(prompt).is_err() {
            self.pending_turns = 0;
            self.clear_queued_messages();
            self.messages_mut().push(Message {
                author: Author::Notice,
                text: t!("ai.claude.disconnected").to_string(),
                time: None,
                images: Vec::new(),
                queue_position: None,
                markdown_view: None,
                thinking_tail: "".into(),
                tools: Vec::new(),
            });
        } else {
            self.mark_next_queued_message_sent();
        }
        self.focus_input(window, cx);
    }

    fn mark_next_queued_message_sent(&mut self) {
        for message in self.messages_mut() {
            if message.queue_position == Some(1) {
                message.queue_position = None;
                break;
            }
        }
        for message in self.messages_mut() {
            if let Some(position) = message.queue_position.as_mut() {
                *position = position.saturating_sub(1);
            }
        }
    }

    fn clear_queued_messages(&mut self) {
        self.queued_prompts.clear();
        for message in self.messages_mut() {
            message.queue_position = None;
        }
    }

    fn edit_queued_message(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt_index) = position.checked_sub(1) else {
            return;
        };
        if self.queued_prompts.remove(prompt_index).is_none() {
            return;
        }
        let Some(message_index) = self
            .messages
            .iter()
            .position(|message| message.queue_position == Some(position))
        else {
            return;
        };
        let message = self.messages_mut().remove(message_index);
        for path in message.images {
            if !self.attachments.contains(&path) {
                self.attachments.push(path);
            }
        }
        self.shift_queue_after_removal(position);
        self.pending_turns = self.pending_turns.saturating_sub(1);
        self.busy = self.pending_turns > 0;
        self.set_input_source(&message.text, window, cx);
        self.focus_input(window, cx);
        self.sync_message_scroller(cx);
        cx.notify();
    }

    fn adjust_queued_message(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(prompt_index) = position.checked_sub(1) else {
            return;
        };
        let Some(prompt) = self.queued_prompts.remove(prompt_index) else {
            return;
        };
        self.queued_prompts.push_front(prompt);
        for message in self.messages_mut() {
            if let Some(queued) = message.queue_position {
                message.queue_position = Some(if queued == position {
                    1
                } else if queued < position {
                    queued + 1
                } else {
                    queued
                });
            }
        }
        self.interrupt_current(t!("ai.claude.adjusting"), cx);
        cx.notify();
    }

    fn close_queued_message(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(prompt_index) = position.checked_sub(1) else {
            return;
        };
        if self.queued_prompts.remove(prompt_index).is_none() {
            return;
        }
        if let Some(message_index) = self
            .messages
            .iter()
            .position(|message| message.queue_position == Some(position))
        {
            self.messages_mut().remove(message_index);
        }
        self.shift_queue_after_removal(position);
        self.pending_turns = self.pending_turns.saturating_sub(1);
        self.busy = self.pending_turns > 0;
        self.sync_message_scroller(cx);
        cx.notify();
    }

    fn shift_queue_after_removal(&mut self, removed: usize) {
        for message in self.messages_mut() {
            if let Some(position) = message.queue_position.as_mut()
                && *position > removed
            {
                *position -= 1;
            }
        }
    }

    fn interrupt_current(&mut self, status: SharedString, cx: &mut Context<Self>) {
        if !self.busy || self.stopping {
            return;
        }
        let Some(claude) = &self.claude else {
            return;
        };
        if claude
            .commands
            .try_send(claude::ClaudeCommand::Interrupt)
            .is_err()
        {
            return;
        }
        self.stopping = true;
        self.clear_permission();
        self.claude_status = Some((status, cx.theme().warning));
        cx.notify();
    }

    fn stop_response(&mut self, cx: &mut Context<Self>) {
        self.interrupt_current(t!("ai.claude.stopping"), cx);
    }

    fn attach_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t!("ai.attachments.choose")),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            this.update_in(cx, |panel, _, cx| {
                for path in paths {
                    if !panel.attachments.contains(&path) {
                        panel.attachments.push(path);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
            cx.notify();
        }
    }

    fn reset_session(&mut self, cx: &mut Context<Self>) {
        self.claude = None;
        self.resume_session = self.session_id.is_some();
        self.claude_status = None;
        self.context_usage = None;
        self.streaming = None;
        self.busy = false;
        self.pending_turns = 0;
        self.clear_queued_messages();
        self.stopping = false;
        self.clear_permission();
        cx.notify();
    }

    fn start_claude(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.claude.is_some() {
            return;
        }
        let ai = self.ai(cx);
        let cwd = claude::working_directory(ai.working_directory);
        let command = claude::claude_command();
        let (prompt_tx, prompt_rx) = async_channel::unbounded();
        let (command_tx, command_rx) = async_channel::unbounded();
        let (permission_tx, permission_rx) = async_channel::unbounded();
        let (event_tx, event_rx) = async_channel::unbounded();
        let connection_events = event_tx.clone();
        let session_id = *self.session_id.get_or_insert_with(Uuid::new_v4);
        let resume = self.resume_session;
        self.resume_session = false;
        let worker = claude::spawn(
            command,
            cwd,
            self.model.clone(),
            self.background,
            ai.confirmation,
            session_id,
            resume,
            prompt_rx,
            command_rx,
            permission_rx,
            connection_events,
        );
        let events = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(AI_STREAM_COMMIT_INTERVAL)
                    .await;
                let mut pending = Vec::new();
                while let Ok(event) = event_rx.try_recv() {
                    pending.push(event);
                }
                let pending = coalesce_claude_events(pending);
                if !pending.is_empty()
                    && this
                        .update_in(cx, |panel, window, cx| {
                            panel.on_claude_events(pending, window, cx)
                        })
                        .is_err()
                {
                    break;
                }
                if event_rx.is_closed() {
                    break;
                }
            }
        });
        self.claude = Some(ClaudeBridge {
            prompts: prompt_tx,
            commands: command_tx,
            permissions: permission_tx,
            _worker: worker,
            _events: events,
        });
    }

    fn on_claude_events(
        &mut self,
        events: Vec<ClaudeEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut remeasure = None;
        for event in events {
            match event {
                ClaudeEvent::Connecting => {
                    self.claude_status = Some((t!("ai.claude.connecting"), cx.theme().warning))
                }
                ClaudeEvent::Ready => {
                    self.claude_status = Some((t!("ai.claude.ready"), cx.theme().success))
                }
                ClaudeEvent::Thinking(thinking) => {
                    let index = match self.streaming {
                        Some(index) => index,
                        None => {
                            self.messages_mut().push(Message {
                                author: Author::Assistant,
                                text: "".into(),
                                time: None,
                                images: Vec::new(),
                                queue_position: None,
                                markdown_view: Some(Rc::new(RefCell::new(
                                    md::render::MarkdownView::seeded(),
                                ))),
                                thinking_tail: "".into(),
                                tools: Vec::new(),
                            });
                            let index = self.messages.len() - 1;
                            self.streaming = Some(index);
                            index
                        }
                    };
                    if let Some(message) = self.messages_mut().get_mut(index) {
                        let thinking = format!("{}{}", message.thinking_tail, thinking);
                        if let Some(tail) = streaming_tail(&thinking) {
                            message.thinking_tail = tail;
                            remeasure = Some(index);
                        }
                    }
                }
                ClaudeEvent::Text(text) => {
                    let index = match self.streaming {
                        Some(index) => index,
                        None => {
                            self.messages_mut().push(Message {
                                author: Author::Assistant,
                                text: "".into(),
                                time: None,
                                images: Vec::new(),
                                queue_position: None,
                                markdown_view: Some(Rc::new(RefCell::new(
                                    md::render::MarkdownView::seeded(),
                                ))),
                                thinking_tail: "".into(),
                                tools: Vec::new(),
                            });
                            let index = self.messages.len() - 1;
                            self.streaming = Some(index);
                            index
                        }
                    };
                    if let Some(message) = self.messages_mut().get_mut(index) {
                        message.text.push_str(&text);
                        remeasure = Some(index);
                    }
                }
                ClaudeEvent::ToolCall { id, name, input } => {
                    let pending_name = name.clone();
                    if let Some(index) = self.streaming.take()
                        && self
                            .messages
                            .get(index)
                            .is_some_and(|message| message.text.is_empty())
                    {
                        self.messages_mut().remove(index);
                    }
                    let file_change = tool_file_change(&pending_name, &input);
                    let diff_state = file_change
                        .as_ref()
                        .and_then(|change| DiffFile::parse(&change.patch).ok())
                        .map(|files| cx.new(|cx| DiffState::new(files, cx)));
                    let tool = ToolCall {
                        id,
                        name,
                        summary: tool_summary(&pending_name, &input).into(),
                        input_text: tool_input_text(&pending_name, &input).into(),
                        output: None,
                        status: ToolStatus::Running,
                        file_change,
                        diff_state,
                    };
                    self.tool_scrolls
                        .insert(format!("{}:input", tool.id), ScrollHandle::new());
                    self.tool_scrolls
                        .insert(format!("{}:output", tool.id), ScrollHandle::new());
                    remeasure = Some(append_tool_call(self.messages_mut(), tool));
                }
                ClaudeEvent::ToolResult {
                    id,
                    content,
                    is_error,
                } => {
                    let messages = self.messages_mut();
                    for (index, message) in messages.iter_mut().enumerate().rev() {
                        let Some(tool) = message.tools.iter_mut().rev().find(|tool| tool.id == id)
                        else {
                            continue;
                        };
                        tool.output = Some(truncated_tool_text(&content).into());
                        tool.status = if is_error {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Succeeded
                        };
                        remeasure = Some(index);
                        break;
                    }
                }
                ClaudeEvent::Permission {
                    request_id,
                    tool_name,
                    input,
                    reason,
                } => {
                    self.open_permission_questionnaire(
                        PendingPermission {
                            request_id,
                            tool_name,
                            input,
                            reason,
                        },
                        window,
                        cx,
                    );
                }
                ClaudeEvent::PermissionCancelled { request_id } => {
                    if self
                        .permission
                        .as_ref()
                        .is_some_and(|permission| permission.request_id == request_id)
                    {
                        self.clear_permission();
                    }
                }
                ClaudeEvent::ContextUsage(usage) => {
                    self.context_usage = Some(usage);
                }
                ClaudeEvent::Done => {
                    self.pending_turns = self.pending_turns.saturating_sub(1);
                    self.stopping = false;
                    self.clear_permission();
                    if let Some(index) = self.streaming.take()
                        && self
                            .messages
                            .get(index)
                            .is_some_and(|message| message.text.is_empty())
                    {
                        self.messages_mut().remove(index);
                    }
                    self.dispatch_next_queued(window, cx);
                    self.busy = self.pending_turns > 0;
                }
                ClaudeEvent::Error(error) => {
                    self.pending_turns = 0;
                    self.clear_queued_messages();
                    self.busy = false;
                    self.stopping = false;
                    self.streaming = None;
                    self.clear_permission();
                    self.resume_session = self.session_id.is_some();
                    self.claude_status = Some((t!("ai.claude.failed"), cx.theme().danger));
                    self.messages_mut().push(Message {
                        author: Author::Notice,
                        text: t!("ai.claude.error", error = error).to_string(),
                        time: None,
                        images: Vec::new(),
                        queue_position: None,
                        markdown_view: None,
                        thinking_tail: "".into(),
                        tools: Vec::new(),
                    });
                }
            }
        }
        self.sync_message_scroller(cx);
        if let Some(index) = remeasure {
            self.message_scroller.update(cx, |state, cx| {
                state.remeasure_items(index..index + 1, cx);
            });
        }
        cx.notify();
    }

    fn answer_permission(
        &mut self,
        allow: bool,
        note: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.permission.take().is_none() {
            return;
        }
        self.permission_state = None;
        self.permission_subscription = None;
        let decision = if allow {
            claude::PermissionDecision::Allow
        } else {
            claude::PermissionDecision::Deny(
                note.filter(|note| !note.trim().is_empty())
                    .unwrap_or_else(|| "The user denied this tool call in ShellRS.".to_string()),
            )
        };
        if let Some(claude) = &self.claude {
            let _ = claude.permissions.try_send(decision);
        }
        self.focus_input(window, cx);
        cx.notify();
    }

    fn clear_permission(&mut self) {
        self.permission = None;
        self.permission_state = None;
        self.permission_subscription = None;
    }

    fn open_permission_questionnaire(
        &mut self,
        permission: PendingPermission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = t!(tool_title_key(&permission.tool_name));
        let summary = tool_summary(&permission.tool_name, &permission.input);
        let mut description = summary;
        if let Some(reason) = &permission.reason {
            description.push('\n');
            description.push_str(&t!("ai.permission.reason", reason = reason));
        }
        let note = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("ai.permission.input_placeholder"))
        });
        let items = vec![
            QuestionnaireItemDefinition::new(
                "decision",
                t!("ai.permission.headline", tool = title),
            )
            .with_required(true)
            .with_description(description)
            .with_choices([
                QuestionnaireChoiceDefinition::new("allow", t!("ai.permission.allow_once"))
                    .with_description(t!("ai.permission.allow_description")),
                QuestionnaireChoiceDefinition::new("reject", t!("ai.permission.reject"))
                    .with_description(t!("ai.permission.reject_description")),
            ])
            .with_input(QuestionnaireInputDefinition::new(
                note,
                t!("ai.permission.input_label"),
            )),
        ];
        let state = cx.new(|cx| {
            QuestionnaireState::new(items, cx).expect("permission questionnaire is valid")
        });
        let subscription = cx.subscribe_in(
            &state,
            window,
            |this, _state, event: &QuestionnaireEvent, window, cx| {
                let submission = match event {
                    QuestionnaireEvent::Completed(submission)
                    | QuestionnaireEvent::Submit(submission) => submission,
                    _ => return,
                };
                let allow = submission
                    .answer("decision")
                    .and_then(|answer| answer.choices().first())
                    .is_some_and(|choice| choice.as_ref() == "allow");
                let note = submission
                    .answer("decision")
                    .and_then(|answer| answer.freeform())
                    .map(|note| note.to_string());
                this.answer_permission(allow, note, window, cx);
            },
        );
        self.permission = Some(permission);
        self.permission_state = Some(state.clone());
        self.permission_subscription = Some(subscription);
        cx.defer_in(window, move |_, window, cx| {
            state.update(cx, |state, cx| {
                let _ = state.focus_current_item(window, cx);
            });
        });
    }

    fn toggle_tool_group(
        &mut self,
        id: &str,
        currently_expanded: bool,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.expanded_tool_groups
            .insert(id.to_string(), !currently_expanded);
        self.message_scroller.update(cx, |state, cx| {
            state.remeasure_items(index..index + 1, cx);
        });
        cx.notify();
    }

    fn toggle_tool(&mut self, id: &str, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_tools.remove(id) {
            self.expanded_tools.insert(id.to_string());
        }
        self.message_scroller.update(cx, |state, cx| {
            state.remeasure_items(index..index + 1, cx);
        });
        cx.notify();
    }

    fn sync_message_scroller(&mut self, cx: &mut Context<Self>) {
        let count = self.messages.len();
        self.message_scroller.update(cx, |state, cx| {
            let current = state.item_count();
            if count > current {
                state.append(count - current, cx);
            } else if count < current {
                state.reset(count, cx);
            }
        });
    }

    /// Start a new conversation with the same host: the old one stays gone
    /// until there is a real history store to keep it in.
    pub fn new_conversation(&mut self, cx: &mut Context<Self>) {
        self.messages_mut().clear();
        self.expanded_tools.clear();
        self.expanded_tool_groups.clear();
        self.tool_scrolls.clear();
        self.claude = None;
        self.session_id = None;
        self.resume_session = false;
        self.claude_status = None;
        self.context_usage = None;
        self.streaming = None;
        self.busy = false;
        self.pending_turns = 0;
        self.clear_queued_messages();
        self.stopping = false;
        self.attachments.clear();
        self.model = None;
        let scroller = self.message_scroller.clone();
        scroller.update(cx, |state, cx| state.reset(0, cx));
        cx.notify();
    }

    pub(crate) fn current_cwd(&self, cx: &App) -> PathBuf {
        claude::working_directory(self.ai(cx).working_directory)
    }

    pub(crate) fn current_session(&self) -> Option<Uuid> {
        self.session_id
    }

    pub(crate) fn adopt_history(
        &mut self,
        conversation: history::Conversation,
        cx: &mut Context<Self>,
    ) {
        self.claude = None;
        self.session_id = Some(conversation.summary.id);
        self.expanded_tools.clear();
        self.expanded_tool_groups.clear();
        self.tool_scrolls.clear();
        self.resume_session = true;
        self.claude_status = None;
        self.context_usage = None;
        self.streaming = None;
        self.busy = false;
        self.pending_turns = 0;
        self.clear_queued_messages();
        self.stopping = false;
        self.attachments.clear();
        self.messages = Rc::new(
            conversation
                .messages
                .into_iter()
                .map(|message| {
                    let author = match message.role {
                        history::Role::User => Author::User,
                        history::Role::Assistant => Author::Assistant,
                    };
                    let text = message.text;
                    let markdown_view =
                        Some(Rc::new(RefCell::new(md::render::MarkdownView::new())));
                    Message {
                        author,
                        text,
                        time: None,
                        images: Vec::new(),
                        queue_position: None,
                        markdown_view,
                        thinking_tail: "".into(),
                        tools: Vec::new(),
                    }
                })
                .collect(),
        );
        let count = self.messages.len();
        let scroller = self.message_scroller.clone();
        scroller.update(cx, |state, cx| {
            state.reset(count, cx);
            state.scroll_to_end(cx);
        });
        cx.notify();
    }

    fn model_label(&self) -> SharedString {
        self.model
            .as_ref()
            .map(|model| SharedString::from(model.display_name().to_string()))
            .unwrap_or_else(|| t!("ai.model.default"))
    }

    /// What the panel title says about the assistant now.
    pub fn status(&self, cx: &App) -> (SharedString, Hsla) {
        if self.permission.is_some() {
            return (t!("ai.permission.waiting"), cx.theme().primary);
        }
        if let Some((status, color)) = &self.claude_status {
            return (status.clone(), *color);
        }
        let ai = self.ai(cx);
        if !ai.enabled {
            (t!("ai.status.disabled"), cx.theme().muted_foreground)
        } else if active_environment(&ai).is_none() {
            (t!("ai.status.unconfigured"), cx.theme().warning)
        } else {
            (t!("ai.status.ready"), cx.theme().success)
        }
    }

    fn markdown_selection_input(&self) -> impl IntoElement {
        let selection = self.markdown_selection.clone();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| md::render::install_selection_input(window, &selection),
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0))
    }

    fn render_empty(&self, cx: &App) -> impl IntoElement {
        v_flex()
            .id("ai-empty")
            .test_support()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .w_12()
                    .h_12()
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(CatalogIcon::Sparkles)
                            .large()
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("ai.empty.title")),
            )
            .child(
                div()
                    .max_w_72()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("ai.empty.description")),
            )
    }

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let input_text = self.input_source(cx);
        let input_empty = input_text.trim().is_empty();
        let show_placeholder = input_text.is_empty();
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let placeholder = t!("ai.composer.placeholder");
        let empty = input_empty && self.attachments.is_empty();
        let busy = self.busy;
        let stopping = self.stopping;
        let show_stop = busy && empty;
        let model = self.model.clone();
        let model_label = self.model_label();
        let confirmation = self.ai(cx).confirmation;
        let settings = self.settings.clone();
        let send = cx.entity();
        let stop = cx.entity();
        let attach = cx.entity();
        let attachments = self.attachments.clone();
        let skill_menu = self.render_skill_menu_overlay(cx);
        let bounds_cell = self.skill_menu_bounds.clone();
        let cwd = self.current_cwd(cx);
        let paste_panel = cx.entity();
        let context_meter = context_meter(self.context_usage, cx);

        let toolbar = h_flex()
            .absolute()
            .left_3()
            .right_3()
            .bottom_2()
            .items_center()
            .gap_1()
            .child(
                div().flex_shrink_0().child(
                    Button::new("ai-attach")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip(t!("ai.composer.attach"))
                        .on_click(move |_, window, cx| {
                            attach.update(cx, |panel, cx| panel.attach_files(window, cx));
                        }),
                ),
            )
            .child(
                div()
                    .min_w_0()
                    .max_w(rems(8.))
                    .overflow_hidden()
                    .child(confirmation_menu(
                        cx.entity(),
                        settings.clone(),
                        confirmation,
                    )),
            )
            .child(div().flex_1().min_w_0())
            .child(
                div()
                    .min_w_0()
                    .max_w(rems(14.))
                    .overflow_hidden()
                    .child(model_menu(cx.entity(), model, model_label)),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .when(show_stop, |controls| {
                        let stop_button = Button::new("ai-stop")
                            .danger()
                            .small()
                            .rounded(cx.theme().radius)
                            .w(rems(1.75))
                            .h(rems(1.75))
                            .icon(Icon::new(IconName::Square))
                            .accessibility_label(t!("ai.composer.stop"))
                            .disabled(stopping)
                            .loading(stopping)
                            .on_click(move |_, _, cx| {
                                stop.update(cx, |panel, cx| panel.stop_response(cx));
                            });
                        controls.child(context_progress_action(
                            "ai-context-progress",
                            stop_button,
                            &context_meter,
                            cx,
                        ))
                    })
                    .when(!show_stop, |controls| {
                        let send_button = Button::new("ai-send")
                            .small()
                            .rounded(cx.theme().radius)
                            .w(rems(1.75))
                            .h(rems(1.75))
                            .icon(IconName::ArrowUp)
                            .accessibility_label(t!("ai.composer.send"))
                            .disabled(empty)
                            .when(empty, |button| button.secondary())
                            .when(!empty, |button| button.primary())
                            .on_click(move |_, window, cx| {
                                send.update(cx, |panel, cx| panel.send(window, cx));
                            });
                        controls.child(context_progress_action(
                            "ai-context-progress",
                            send_button,
                            &context_meter,
                            cx,
                        ))
                    }),
            );

        let composer = v_flex()
            .id("ai-composer")
            .test_support()
            .relative()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().input_background())
            .shadow_sm()
            .when(focused, |this| this.focus_ring_style(window, cx))
            .child(
                canvas(
                    move |bounds, _, _| bounds_cell.set(Some(bounds)),
                    |_, _, _, _| (),
                )
                .absolute()
                .inset_0(),
            )
            .when(!attachments.is_empty(), |composer| {
                let panel = cx.entity();
                composer.child(
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .gap_1()
                        .px_3()
                        .pt_2()
                        .children(attachments.iter().enumerate().map(|(ix, path)| {
                            render_composer_attachment(ix, path, &cwd, panel.clone(), cx)
                        })),
                )
            })
            .child(
                div()
                    .w_full()
                    .px_3()
                    .pt_2()
                    // Keep the last lines clear of the controls that float
                    // at the bottom of the same input frame.
                    .pb(rems(3.5))
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .when(show_placeholder, |this| {
                                this.child(
                                    div()
                                        .id("ai-composer-placeholder")
                                        .test_support()
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .right_0()
                                        // Match the textarea's `Size::Medium` text
                                        // inset, plus the 10px its text element
                                        // reserves on the right before wrapping.
                                        .pl(px(10.))
                                        .pr(px(20.))
                                        .pt(px(8.))
                                        .text_sm()
                                        .line_height(rems(1.25))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(placeholder.clone()),
                                )
                            })
                            .child(
                                Textarea::new(&self.input)
                                    .appearance(false)
                                    .bordered(false)
                                    .aria_label(placeholder)
                                    .w_full()
                                    .on_paste(move |item, window, cx| {
                                        paste_composer_attachments(&paste_panel, item, window, cx)
                                    }),
                            ),
                    ),
            )
            .child(toolbar);

        v_flex()
            .w_full()
            .gap_2()
            .child(composer)
            .when_some(skill_menu, |this, menu| this.child(menu))
            .into_any_element()
    }

    fn render_skill_menu_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.skill_menu_dismissed {
            return None;
        }
        let trigger = self.skill_trigger.as_ref()?;
        let skills = self
            .filtered_skills(&trigger.query)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if skills.is_empty() {
            return None;
        }
        let bounds = self.skill_menu_bounds.get()?;
        let theme = cx.theme().clone();
        let selected = self.skill_menu_index.min(skills.len().saturating_sub(1));
        let panel = cx.entity();
        let scroll = self.skill_menu_scroll.clone();
        let rows = skills
            .iter()
            .enumerate()
            .map(|(index, skill)| {
                let panel = panel.clone();
                let slug = skill.slug.clone();
                let command = format!("/{}", skill.slug);
                h_flex()
                    .id(SharedString::from(format!("ai-skill-{}", skill.slug)))
                    .test_support()
                    .aria_label(skill.name.clone())
                    .w_full()
                    .h(rems(1.875))
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded(theme.radius)
                    .when(index == selected, |row| row.bg(theme.accent))
                    .hover(|style| style.bg(theme.list_hover))
                    .on_click(move |_, window, cx| {
                        panel.update(cx, |panel, cx| panel.insert_skill(&slug, window, cx));
                    })
                    .child(
                        Icon::new(CatalogIcon::Sparkles)
                            .xsmall()
                            .text_color(theme.primary),
                    )
                    .child(
                        div()
                            .flex_none()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(command),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(skill.description.clone().unwrap_or_default()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .h(rems(1.125))
                            .px_1p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child(skill.category.clone()),
                    )
            })
            .collect::<Vec<_>>();

        let popup = div()
            .id("ai-skill-menu")
            .test_support()
            .occlude()
            .w(bounds.size.width)
            .max_h(rems(18.875))
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.skill_menu_dismissed = true;
                cx.notify();
            }))
            .child(
                div()
                    .w_full()
                    .max_h(rems(18.875))
                    .relative()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("ai-skill-list")
                            .w_full()
                            .max_h(rems(18.875))
                            .overflow_y_scroll()
                            .track_scroll(&scroll)
                            .p_1()
                            .children(rows),
                    )
                    .child(Scrollbar::vertical(&scroll)),
            );

        Some(
            deferred(
                anchored()
                    .position(point(bounds.origin.x, bounds.origin.y - px(6.)))
                    .anchor(Anchor::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(popup),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_permission(
        &self,
        permission: PendingPermission,
        state: Entity<QuestionnaireState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let detail = tool_input_text(&permission.tool_name, &permission.input);

        v_flex()
            .id("ai-permission")
            .test_support()
            .aria_label(t!("ai.permission.waiting"))
            .w_full()
            .gap_3()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(IconName::Info).small().text_color(theme.primary))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(t!("ai.permission.waiting")),
                    ),
            )
            .child(
                Questionnaire::new(&state)
                    .small()
                    .w_full()
                    .child(QuestionnaireProgress::new(&state))
                    .child(
                        QuestionnaireItem::new(&state, "decision")
                            .small()
                            .child(QuestionnaireTitle::new(&state, "decision"))
                            .child(QuestionnaireDescription::new(&state, "decision"))
                            .child(
                                div()
                                    .id("ai-permission-detail")
                                    .test_support()
                                    .w_full()
                                    .max_h(rems(10.))
                                    .overflow_y_scroll()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .p_2()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_xs()
                                    .child(detail),
                            )
                            .child(
                                QuestionnaireChoices::new(&state, "decision")
                                    .small()
                                    .child(
                                        QuestionnaireChoice::new(&state, "decision", "allow")
                                            .child(
                                                QuestionnaireChoiceDescription::new()
                                                    .child(t!("ai.permission.allow_description")),
                                            ),
                                    )
                                    .child(
                                        QuestionnaireChoice::new(&state, "decision", "reject")
                                            .child(
                                                QuestionnaireChoiceDescription::new()
                                                    .child(t!("ai.permission.reject_description")),
                                            ),
                                    )
                                    .child(QuestionnaireInput::new(&state, "decision")),
                            )
                            .child(QuestionnaireError::new(&state, "decision")),
                    )
                    .child(
                        QuestionnaireActions::new(&state)
                            .child(QuestionnairePrevious::new(&state))
                            .child(QuestionnaireSkip::new(&state))
                            .child(QuestionnaireNext::new(&state))
                            .child(QuestionnaireSubmit::new(&state)),
                    ),
            )
            .into_any_element()
    }
}

fn coalesce_claude_events(events: Vec<ClaudeEvent>) -> Vec<ClaudeEvent> {
    let mut coalesced = Vec::with_capacity(events.len());
    for event in events {
        match event {
            ClaudeEvent::Text(text) => {
                if let Some(ClaudeEvent::Text(previous)) = coalesced.last_mut() {
                    previous.push_str(&text);
                } else {
                    coalesced.push(ClaudeEvent::Text(text));
                }
            }
            event => coalesced.push(event),
        }
    }
    coalesced
}

fn streaming_tail(text: &str) -> Option<SharedString> {
    const TAIL_CHARS: usize = 48;
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    let count = line.chars().count();
    let tail: String = line
        .chars()
        .skip(count.saturating_sub(TAIL_CHARS))
        .collect();
    (count > TAIL_CHARS)
        .then(|| SharedString::from(format!("…{tail}")))
        .or_else(|| (!tail.is_empty()).then(|| SharedString::from(tail)))
}

fn render_message_row(
    message: Option<Message>,
    ix: usize,
    context: &MessageRowContext,
    panel: &Entity<AiAssistantPanel>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let MessageRowContext {
        streaming,
        cwd,
        markdown_selection,
        ..
    } = context;
    let Some(message) = message else {
        return Empty.into_any_element();
    };
    let id = SharedString::from(format!("ai-message-{ix}"));
    let thinking_background = cx.theme().primary.opacity(0.08);
    match message.author {
        Author::User => {
            let group = SharedString::from(format!("ai-user-message-{ix}"));
            let copied = message.text.clone();
            let queued_copy = copied.clone();
            let bubble = v_flex()
                .min_w_0()
                .when(message.text.contains('\n'), |bubble| {
                    bubble.w(relative(0.75))
                })
                .when(!message.text.contains('\n'), |bubble| bubble.max_w_3_4())
                .when(message.queue_position.is_some(), |bubble| {
                    bubble.min_w(rems(QUEUED_MESSAGE_ACTIONS_MIN_WIDTH))
                })
                .rounded(cx.theme().radius)
                .bg(thinking_background)
                .when_some(message.queue_position, |bubble, position| {
                    bubble.gap_1().child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(CatalogIcon::Clock).xsmall())
                            .child(t!("ai.message.queue_position", position = position)),
                    )
                })
                .px_3()
                .py_2()
                .text_sm()
                .child(render_message_markdown(
                    &message,
                    ix,
                    false,
                    cwd,
                    markdown_selection,
                    cx,
                ));
            v_flex()
                .id(id)
                .w_full()
                .items_end()
                .gap_1()
                .group(group.clone())
                .children(message.images.iter().enumerate().map(|(image_ix, path)| {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let preview_path = path.clone();
                    let preview_name = name.clone();
                    let preview_cwd = cwd.clone();
                    let copy_path = local_image_path(&path.to_string_lossy(), cwd)
                        .unwrap_or_else(|| path.clone());
                    div()
                        .id(ElementId::Name(
                            format!("ai-message-{ix}-image-{image_ix}").into(),
                        ))
                        .test_support()
                        .on_click(move |_, window, cx| {
                            open_ai_image_preview(
                                &preview_path.to_string_lossy(),
                                &preview_name,
                                &preview_cwd,
                                window,
                                cx,
                            )
                        })
                        .tooltip(move |window, cx| {
                            Tooltip::new(SharedString::from(name.clone())).build(window, cx)
                        })
                        .context_menu(move |menu, _, _| {
                            menu.menu_with_icon(
                                t!("ai.image.copy"),
                                Icon::new(CatalogIcon::ClipboardCopy),
                                Box::new(CopyAiImage(copy_path.clone())),
                            )
                        })
                        .child(
                            img(markdown_image_source(&path.to_string_lossy(), cwd))
                                .object_fit(ObjectFit::Contain)
                                .max_w(rems(18.))
                                .max_h(rems(12.))
                                .rounded(cx.theme().radius),
                        )
                }))
                .child(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .child(bubble.into_any_element()),
                )
                .when_some(message.queue_position, |row, position| {
                    let edit_panel = panel.clone();
                    let adjust_panel = panel.clone();
                    let close_panel = panel.clone();
                    row.child(
                        h_flex()
                            .min_w(rems(QUEUED_MESSAGE_ACTIONS_MIN_WIDTH))
                            .max_w_3_4()
                            .justify_end()
                            .items_center()
                            .gap_2()
                            .child(
                                h_flex()
                                    .invisible()
                                    .group_hover(group.clone(), |style| style.visible())
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "ai-queue-edit-{ix}"
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .label(t!("ai.message.edit"))
                                        .on_click(
                                            move |_, window, cx| {
                                                edit_panel.update(cx, |panel, cx| {
                                                    panel.edit_queued_message(position, window, cx)
                                                });
                                            },
                                        ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "ai-queue-adjust-{ix}"
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .label(t!("ai.message.adjust_direction"))
                                        .on_click(
                                            move |_, _, cx| {
                                                adjust_panel.update(cx, |panel, cx| {
                                                    panel.adjust_queued_message(position, cx)
                                                });
                                            },
                                        ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "ai-queue-close-{ix}"
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .label(t!("ai.message.close"))
                                        .on_click(
                                            move |_, _, cx| {
                                                close_panel.update(cx, |panel, cx| {
                                                    panel.close_queued_message(position, cx)
                                                });
                                            },
                                        ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "ai-copy-queued-message-{ix}"
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Copy)
                                        .tooltip(t!("ai.message.copy"))
                                        .on_click(
                                            move |_, window, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    queued_copy.to_string(),
                                                ));
                                                window.push_notification(
                                                    Notification::success(t!("ai.message.copied")),
                                                    cx,
                                                );
                                            },
                                        ),
                                    ),
                            ),
                    )
                })
                .when(message.queue_position.is_none(), |row| {
                    row.child(
                        h_flex()
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(message.time.clone().unwrap_or_default()),
                            )
                            .child(
                                Button::new(SharedString::from(format!("ai-copy-message-{ix}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Copy)
                                    .tooltip(t!("ai.message.copy"))
                                    .on_click(move |_, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copied.to_string(),
                                        ));
                                        window.push_notification(
                                            Notification::success(t!("ai.message.copied")),
                                            cx,
                                        );
                                    }),
                            ),
                    )
                })
                .into_any_element()
        }
        Author::Assistant => {
            let group = SharedString::from(format!("ai-assistant-message-{ix}"));
            let copied = message.text.clone();
            let is_streaming = *streaming == Some(ix);
            let thinking_spinner = window.use_keyed_state(
                SharedString::from(format!("ai-thinking-spinner-{ix}")),
                cx,
                |_, _| ThinkingSpinner,
            );
            let stream_tail = if message.text.is_empty() {
                (!message.thinking_tail.is_empty()).then(|| message.thinking_tail.clone())
            } else {
                streaming_tail(&message.text)
            };
            let body =
                render_message_markdown(&message, ix, is_streaming, cwd, markdown_selection, cx);
            let has_body = !message.text.trim().is_empty();
            v_flex()
                .id(id)
                .w_full()
                .gap_1()
                .group(group.clone())
                .when(is_streaming, |message| {
                    message.child(
                        h_flex()
                            .id(SharedString::from(format!("ai-thinking-{ix}")))
                            .test_support()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .rounded_full()
                            .bg(thinking_background)
                            .px_2()
                            .py_1()
                            .child(thinking_spinner)
                            .when_some(stream_tail, |row, tail| {
                                row.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(tail),
                                )
                            }),
                    )
                })
                .when(has_body, |message| message.child(body))
                .child(
                    h_flex()
                        .invisible()
                        .group_hover(group, |style| style.visible())
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!(
                                "ai-copy-assistant-message-{ix}"
                            )))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip(t!("ai.message.copy"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copied.to_string(),
                                ));
                                window.push_notification(
                                    Notification::success(t!("ai.message.copied")),
                                    cx,
                                );
                            }),
                        ),
                )
                .into_any_element()
        }
        Author::Tool => render_tool_group(&message.tools, ix, context, panel, cx),
        Author::Notice => div()
            .id(id)
            .w_full()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(message.text.clone()))
            .into_any_element(),
    }
}

struct MessageRowContext {
    streaming: Option<usize>,
    live_tool_group: Option<usize>,
    cwd: PathBuf,
    expanded_tools: HashSet<String>,
    expanded_tool_groups: HashMap<String, bool>,
    tool_scrolls: HashMap<String, ScrollHandle>,
    markdown_selection: md::render::TranscriptSelection,
}

fn render_message_markdown(
    message: &Message,
    ix: usize,
    is_streaming: bool,
    message_cwd: &Path,
    selection: &md::render::TranscriptSelection,
    cx: &mut App,
) -> AnyElement {
    let theme = md::theme::Theme::from_component(cx.theme());
    let palette = md::render::Palette::from_theme(&theme);
    let font = TerminalFont::current(cx);
    let text_size = f32::from(font.size);
    let metrics = md::render::Metrics {
        text_size,
        line_height: (text_size * 1.5).round(),
        code_text_size: (text_size - 1.0).max(12.0),
        code_line_height: ((text_size - 1.0) * 1.5).max(18.0).round(),
        block_gap: (text_size * 0.72).round(),
    };
    let image_cwd = message_cwd.to_path_buf();
    let resolver_cwd = image_cwd.clone();
    let body_family = cx.theme().font_family.clone();
    let mono_family = font.family(cx);
    let ctx = md::render::Ctx::new(
        format!("ai-assistant-message-{ix}"),
        &palette,
        metrics,
        selection.clone(),
    )
    .with_fonts(body_family, mono_family)
    .with_image_resolver(Rc::new(move |url| {
        markdown_image_source(url, &resolver_cwd)
    }))
    .with_image_click_handler(Rc::new(move |url, alt, window, cx| {
        open_ai_image_preview(url, alt, &image_cwd, window, cx)
    }))
    .with_streaming_animation(is_streaming && !cx.reduce_motion());

    let Some(view) = message.markdown_view.as_ref() else {
        return md::render::plain_text(
            message.text.clone(),
            md::render::SANS_FAMILY,
            FontWeight::NORMAL,
            palette.text,
            &ctx,
        );
    };

    let mut view = view.borrow_mut();
    view.set_text(&message.text, is_streaming);
    md::render::markdown(&view, &ctx).unwrap_or_else(|| {
        md::render::plain_text(
            message.text.clone(),
            md::render::SANS_FAMILY,
            FontWeight::NORMAL,
            palette.text,
            &ctx,
        )
    })
}

fn render_tool_group(
    tools: &[ToolCall],
    ix: usize,
    context: &MessageRowContext,
    panel: &Entity<AiAssistantPanel>,
    cx: &mut App,
) -> AnyElement {
    if tools.is_empty() {
        return Empty.into_any_element();
    }

    let theme = cx.theme().clone();
    let group_id = tools[0].id.clone();
    let running = tools.iter().any(|tool| tool.status == ToolStatus::Running);
    let live_group = context.live_tool_group == Some(ix);
    let expanded = context
        .expanded_tool_groups
        .get(&group_id)
        .copied()
        .unwrap_or(running || live_group);
    let title = tool_group_title(tools, running || live_group);
    let click_panel = panel.clone();
    let key_panel = panel.clone();
    let click_group = group_id.clone();
    let key_group = group_id.clone();

    let mut cluster = v_flex()
        .id(SharedString::from(format!("ai-tool-group-{group_id}")))
        .test_support()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .id(SharedString::from(format!(
                    "ai-tool-group-toggle-{group_id}"
                )))
                .test_support()
                .aria_label(title.clone())
                .role(Role::Button)
                .tab_index(0)
                .w_full()
                .min_w_0()
                .min_h(rems(1.625))
                .gap_2()
                .items_center()
                .cursor_default()
                .focus_visible(|style| style.text_color(theme.foreground))
                .hover(|style| style.text_color(theme.foreground))
                .on_click(move |_, _, cx| {
                    click_panel.update(cx, |panel, cx| {
                        panel.toggle_tool_group(&click_group, expanded, ix, cx);
                    });
                })
                .on_key_down(move |event: &KeyDownEvent, _, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        key_panel.update(cx, |panel, cx| {
                            panel.toggle_tool_group(&key_group, expanded, ix, cx);
                        });
                        cx.stop_propagation();
                    }
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(title),
                )
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                ),
        );

    if expanded {
        let items = tools.iter().map(|tool| {
            render_tool_activity(
                tool,
                ix,
                &context.expanded_tools,
                &context.tool_scrolls,
                panel,
                &theme,
            )
        });
        cluster = cluster.child(
            v_flex()
                .ml_1()
                .pl_3()
                .pb_1()
                .gap_2()
                .border_l_1()
                .border_color(theme.border)
                .children(items),
        );
    }

    cluster.into_any_element()
}

fn render_tool_activity(
    tool: &ToolCall,
    ix: usize,
    expanded_tools: &HashSet<String>,
    tool_scrolls: &HashMap<String, ScrollHandle>,
    panel: &Entity<AiAssistantPanel>,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let expanded = expanded_tools.contains(&tool.id);
    let file_change = tool.file_change.as_ref();
    let shows_diff = tool.diff_state.is_some();
    let has_detail = !tool.input_text.is_empty() || tool.output.is_some() || shows_diff;
    let row_id = SharedString::from(format!("ai-tool-row-{}", tool.id));
    let item_id = SharedString::from(format!("ai-tool-{}", tool.id));
    let title = t!(tool_title_key(&tool.name));
    let summary = tool.summary.clone();
    let click_panel = panel.clone();
    let key_panel = panel.clone();
    let click_id = tool.id.clone();
    let key_id = tool.id.clone();

    let mut card = v_flex()
        .id(item_id)
        .test_support()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted.opacity(0.35))
        .child(
            h_flex()
                .id(row_id)
                .test_support()
                .aria_label(title.clone())
                .role(if has_detail {
                    Role::Button
                } else {
                    Role::ListItem
                })
                .when(has_detail, |row| row.tab_index(0))
                .w_full()
                .min_w_0()
                .min_h(rems(1.75))
                .px_2()
                .gap_2()
                .items_center()
                .text_xs()
                .when(has_detail, |row| {
                    row.cursor_default()
                        .focus_visible(|style| style.bg(theme.list_hover))
                        .hover(|style| style.bg(theme.list_hover))
                        .active(|style| style.bg(theme.list_active))
                        .on_click(move |_, _, cx| {
                            click_panel.update(cx, |panel, cx| {
                                panel.toggle_tool(&click_id, ix, cx);
                            });
                        })
                        .on_key_down(move |event: &KeyDownEvent, _, cx| {
                            if event.keystroke.modifiers.modified() {
                                return;
                            }
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                key_panel.update(cx, |panel, cx| {
                                    panel.toggle_tool(&key_id, ix, cx);
                                });
                                cx.stop_propagation();
                            }
                        })
                })
                .child(
                    Icon::new(tool_icon(&tool.name))
                        .with_size(px(12.0))
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .child(title),
                )
                .when(!summary.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(summary),
                    )
                })
                .when_some(
                    tool_file_change_stats(tool),
                    |row, (additions, deletions)| {
                        row.child(
                            div()
                                .flex_none()
                                .text_color(theme.success)
                                .child(SharedString::from(format!("+{additions}"))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.danger)
                                .child(SharedString::from(format!("-{deletions}"))),
                        )
                    },
                )
                .when(tool.status == ToolStatus::Running, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .size(rems(0.3125))
                            .rounded_full()
                            .bg(theme.primary),
                    )
                })
                .when(tool.status == ToolStatus::Failed, |row| {
                    row.child(
                        Icon::new(IconName::CircleX)
                            .xsmall()
                            .text_color(theme.danger),
                    )
                })
                .when(has_detail, |row| {
                    row.child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(theme.muted_foreground),
                    )
                }),
        );

    if expanded {
        let output = tool
            .output
            .clone()
            .unwrap_or_else(|| t!("ai.tool.no_output"));
        let mut detail = v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .px_3()
            .py_2();

        if let (Some(state), Some(change)) = (&tool.diff_state, file_change) {
            detail = detail.child(render_tool_diff(&tool.id, state, change, theme));
            if tool.status == ToolStatus::Failed {
                detail = detail.child(tool_detail_section(
                    SharedString::from(format!("ai-tool-output-{}", tool.id)),
                    t!("ai.tool.output"),
                    output,
                    true,
                    tool_scrolls
                        .get(&format!("{}:output", tool.id))
                        .cloned()
                        .unwrap_or_default(),
                    theme,
                ));
            }
        } else {
            detail = detail.child(tool_detail_section(
                SharedString::from(format!("ai-tool-input-{}", tool.id)),
                t!("ai.tool.input"),
                tool.input_text.clone(),
                false,
                tool_scrolls
                    .get(&format!("{}:input", tool.id))
                    .cloned()
                    .unwrap_or_default(),
                theme,
            ));
            if tool.status != ToolStatus::Running {
                detail = detail.child(tool_detail_section(
                    SharedString::from(format!("ai-tool-output-{}", tool.id)),
                    t!("ai.tool.output"),
                    output,
                    tool.status == ToolStatus::Failed,
                    tool_scrolls
                        .get(&format!("{}:output", tool.id))
                        .cloned()
                        .unwrap_or_default(),
                    theme,
                ));
            }
        }
        card = card.child(detail);
    }

    card.into_any_element()
}

fn tool_file_change_stats(tool: &ToolCall) -> Option<(usize, usize)> {
    if tool.status != ToolStatus::Succeeded {
        return None;
    }
    let change = tool.file_change.as_ref()?;
    (change.additions > 0 || change.deletions > 0).then_some((change.additions, change.deletions))
}

fn render_tool_diff(
    tool_id: &str,
    state: &Entity<DiffState>,
    change: &ToolFileChange,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let visible_rows = change.additions + change.deletions + 4;
    let height = (visible_rows as f32 * 1.35).clamp(8.0, 25.0);

    v_flex()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(change.path.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.success)
                        .child(SharedString::from(format!("+{}", change.additions))),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.danger)
                        .child(SharedString::from(format!("-{}", change.deletions))),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("ai-tool-diff-{tool_id}")))
                .w_full()
                .h(rems(height))
                .min_h_0()
                .child(
                    Diff::new(state)
                        .header_visible(false)
                        .line_number(true)
                        .syntax_highlight(true)
                        .change_background(true)
                        .soft_wrap(false)
                        .size_full(),
                ),
        )
        .into_any_element()
}

fn append_tool_call(messages: &mut Vec<Message>, tool: ToolCall) -> usize {
    if let Some(message) = messages.last_mut()
        && message.author == Author::Tool
    {
        message.tools.push(tool);
        return messages.len() - 1;
    }

    messages.push(Message {
        author: Author::Tool,
        text: "".into(),
        time: None,
        images: Vec::new(),
        queue_position: None,
        markdown_view: None,
        thinking_tail: "".into(),
        tools: vec![tool],
    });
    messages.len() - 1
}

fn tool_file_change(name: &str, input: &serde_json::Value) -> Option<ToolFileChange> {
    let path = input
        .get("file_path")
        .and_then(|value| value.as_str())
        .filter(|path| !path.trim().is_empty())?;

    match name {
        "Write" => {
            let content = input.get("content").and_then(|value| value.as_str())?;
            let lines = content.split_terminator('\n').collect::<Vec<_>>();
            if lines.is_empty() {
                return None;
            }
            let mut body = format!("@@ -0,0 +1,{} @@\n", lines.len());
            for line in &lines {
                body.push('+');
                body.push_str(line);
                body.push('\n');
            }
            Some(ToolFileChange {
                path: path.to_owned(),
                additions: lines.len(),
                deletions: 0,
                patch: file_patch(path, &body),
            })
        }
        "Edit" => {
            let old = input.get("old_string").and_then(|value| value.as_str())?;
            let new = input.get("new_string").and_then(|value| value.as_str())?;
            let (body, additions, deletions, _, _) = replacement_hunk(old, new, 1, 1);
            if body.is_empty() {
                return None;
            }
            Some(ToolFileChange {
                path: path.to_owned(),
                additions,
                deletions,
                patch: file_patch(path, &body),
            })
        }
        "MultiEdit" => {
            let edits = input.get("edits").and_then(|value| value.as_array())?;
            let mut body = String::new();
            let mut additions = 0;
            let mut deletions = 0;
            let mut old_base = 1;
            let mut new_base = 1;
            for edit in edits {
                let Some(old) = edit.get("old_string").and_then(|value| value.as_str()) else {
                    continue;
                };
                let Some(new) = edit.get("new_string").and_then(|value| value.as_str()) else {
                    continue;
                };
                let (hunk, add, delete, old_span, new_span) =
                    replacement_hunk(old, new, old_base, new_base);
                if hunk.is_empty() {
                    continue;
                }
                body.push_str(&hunk);
                additions += add;
                deletions += delete;
                old_base += old_span.max(1);
                new_base += new_span.max(1);
            }
            if body.is_empty() {
                return None;
            }
            Some(ToolFileChange {
                path: path.to_owned(),
                additions,
                deletions,
                patch: file_patch(path, &body),
            })
        }
        _ => None,
    }
}

fn file_patch(path: &str, body: &str) -> String {
    format!("--- {path}\n+++ {path}\n{body}")
}

fn replacement_hunk(
    old: &str,
    new: &str,
    old_base: usize,
    new_base: usize,
) -> (String, usize, usize, usize, usize) {
    if old == new {
        return (String::new(), 0, 0, 0, 0);
    }

    let old_lines = old.split_terminator('\n').collect::<Vec<_>>();
    let new_lines = new.split_terminator('\n').collect::<Vec<_>>();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(old_lines.len().min(new_lines.len()).saturating_sub(prefix));
    let context = 3;
    let before = prefix.min(context);
    let after = suffix.min(context);
    let old_middle = &old_lines[prefix..old_lines.len() - suffix];
    let new_middle = &new_lines[prefix..new_lines.len() - suffix];
    let old_start = old_base + prefix - before;
    let new_start = new_base + prefix - before;
    let old_count = before + old_middle.len() + after;
    let new_count = before + new_middle.len() + after;

    let mut hunk = format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n");
    for index in 0..before {
        hunk.push(' ');
        hunk.push_str(old_lines[prefix - before + index]);
        hunk.push('\n');
    }
    for line in old_middle {
        hunk.push('-');
        hunk.push_str(line);
        hunk.push('\n');
    }
    for line in new_middle {
        hunk.push('+');
        hunk.push_str(line);
        hunk.push('\n');
    }
    for index in 0..after {
        hunk.push(' ');
        hunk.push_str(old_lines[old_lines.len() - after + index]);
        hunk.push('\n');
    }

    (
        hunk,
        new_middle.len(),
        old_middle.len(),
        old_count,
        new_count,
    )
}

fn tool_group_title(tools: &[ToolCall], running: bool) -> SharedString {
    if running {
        let tool = tools
            .iter()
            .rev()
            .find(|tool| tool.status == ToolStatus::Running)
            .unwrap_or(&tools[tools.len() - 1]);
        let title = t!(tool_title_key(&tool.name));
        if tool.summary.is_empty() {
            return title;
        }
        return SharedString::from(format!("{title} · {}", tool.summary));
    }

    let mut counts: Vec<(ToolKind, usize)> = Vec::new();
    for tool in tools {
        let kind = tool_kind(&tool.name);
        if let Some((_, count)) = counts.iter_mut().find(|(known, _)| *known == kind) {
            *count += 1;
        } else {
            counts.push((kind, 1));
        }
    }
    let summary = counts
        .into_iter()
        .map(|(kind, count)| {
            t!(
                "ai.tool.group.item",
                kind = t!(tool_kind_noun_key(kind)),
                count = count
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    t!("ai.tool.group.ran", summary = summary)
}

fn tool_detail_section(
    id: SharedString,
    label: SharedString,
    text: SharedString,
    error: bool,
    scroll: ScrollHandle,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let copied = text.clone();

    v_flex()
        .w_full()
        .min_w_0()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background.opacity(0.55))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(div().flex_1())
                .when(!text.is_empty(), |header| {
                    header.child(
                        Button::new(SharedString::from(format!("{id}-copy")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip(t!("ai.message.copy"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copied.to_string(),
                                ));
                                window.push_notification(
                                    Notification::success(t!("ai.message.copied")),
                                    cx,
                                );
                            }),
                    )
                }),
        )
        .child(
            div()
                .relative()
                .w_full()
                .min_w_0()
                .max_h(rems(25.))
                .child(
                    div()
                        .id(SharedString::from(format!("{id}-body")))
                        .w_full()
                        .max_h(rems(25.))
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .p_2()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .whitespace_normal()
                        .when(error, |text| text.text_color(theme.danger))
                        .child(text),
                )
                .child(Scrollbar::vertical(&scroll)),
        )
        .into_any_element()
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" | "PowerShell" => ToolKind::Command,
        "Read" | "ReadImage" => ToolKind::Read,
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => ToolKind::Edit,
        "Grep" | "Glob" | "WebSearch" => ToolKind::Search,
        "WebFetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_kind_noun_key(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Command => "ai.tool.kind.command",
        ToolKind::Read => "ai.tool.kind.read",
        ToolKind::Edit => "ai.tool.kind.edit",
        ToolKind::Search => "ai.tool.kind.search",
        ToolKind::Fetch => "ai.tool.kind.fetch",
        ToolKind::Other => "ai.tool.kind.tool",
    }
}

fn tool_icon(name: &str) -> CatalogIcon {
    match tool_kind(name) {
        ToolKind::Command => CatalogIcon::Terminal,
        ToolKind::Read => CatalogIcon::FileText,
        ToolKind::Edit => CatalogIcon::SquarePen,
        ToolKind::Search => CatalogIcon::FileSearch,
        ToolKind::Fetch => CatalogIcon::Globe,
        ToolKind::Other => CatalogIcon::Sparkles,
    }
}

fn tool_title_key(name: &str) -> &'static str {
    match tool_kind(name) {
        ToolKind::Command => "ai.tool.title.command",
        ToolKind::Read => "ai.tool.title.read",
        ToolKind::Edit => "ai.tool.title.edit",
        ToolKind::Search => "ai.tool.title.search",
        ToolKind::Fetch => "ai.tool.title.fetch",
        ToolKind::Other => "ai.tool.title.generic",
    }
}

fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let detail = ["command", "file_path", "path", "pattern", "query", "url"]
        .into_iter()
        .find_map(|key| input.get(key).and_then(|value| value.as_str()))
        .map(one_line)
        .or_else(|| {
            (!input.is_null() && input.as_object().is_none_or(|object| !object.is_empty()))
                .then(|| one_line(&input.to_string()))
        })
        .unwrap_or_else(|| name.to_string());
    if tool_title_key(name) == "ai.tool.title.generic" && detail != name {
        format!("{name} · {detail}")
    } else {
        detail
    }
}

fn tool_input_text(name: &str, input: &serde_json::Value) -> String {
    let text = input
        .get("command")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
        });
    if text.trim().is_empty() {
        name.to_string()
    } else {
        text
    }
}

fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 160 {
        let mut clipped: String = line.chars().take(160).collect();
        clipped.push('…');
        clipped
    } else {
        line
    }
}

fn truncated_tool_text(text: &str) -> String {
    const LIMIT: usize = 32 * 1024;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[derive(serde::Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let home = dirs::home_dir();
    let mut skills = Vec::new();
    let mut seen = HashSet::new();
    let mut sources = vec![
        (cwd.join(".claude").join("skills"), "Project".to_string()),
        (cwd.join(".agents").join("skills"), "Project".to_string()),
    ];
    if let Some(home) = &home {
        sources.extend([
            (home.join(".claude").join("skills"), "Claude".to_string()),
            (home.join(".agents").join("skills"), "Agents".to_string()),
            (home.join(".codex").join("skills"), "Codex".to_string()),
        ]);
    }
    for (directory, category) in sources {
        collect_skill_directory(&directory, &category, &mut seen, &mut skills);
    }
    skills.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.name.cmp(&right.name))
    });
    skills
}

fn collect_skill_directory(
    directory: &Path,
    category: &str,
    seen: &mut HashSet<String>,
    skills: &mut Vec<Skill>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(slug) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if seen.contains(&slug) {
            continue;
        }
        let file = path.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        seen.insert(slug.clone());
        let front = parse_skill_frontmatter(&text);
        skills.push(Skill {
            slug,
            name: front
                .as_ref()
                .and_then(|front| front.name.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }),
            description: front.and_then(|front| front.description),
            category: category.to_string(),
        });
    }
}

fn parse_skill_frontmatter(text: &str) -> Option<SkillFrontmatter> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let end = rest.find("\n---")?;
    serde_yaml_ng::from_str(&rest[..end]).ok()
}

impl Focusable for AiAssistantPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AiAssistantPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.host.is_none() {
            return v_flex().size_full().into_any_element();
        }
        self.sync_skill_trigger(cx);

        v_flex()
            .id("ai-assistant")
            .test_support()
            .aria_label(t!("ai.title"))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                let selected = this.markdown_selection.selection.borrow().selected_text();
                if let Some(text) = selected {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                } else {
                    cx.propagate();
                }
            }))
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(div().size_full().px_3().pb(rems(10.)).child({
                        let mut content = div()
                            .relative()
                            .size_full()
                            .overflow_hidden()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    // The composer is an absolute child of this
                                    // content area. A click inside it already
                                    // focuses the Textarea, so do not let the
                                    // message-area fallback steal that focus.
                                    if !this.input.read(cx).focus_handle(cx).is_focused(window) {
                                        window.focus(&this.focus_handle, cx);
                                    }
                                }),
                            );
                        if self.messages.is_empty() {
                            content = content.child(self.render_empty(cx));
                        } else {
                            let messages = self.messages.clone();
                            let cwd = self.current_cwd(cx);
                            let expanded_tools = self.expanded_tools.clone();
                            let expanded_tool_groups = self.expanded_tool_groups.clone();
                            let tool_scrolls = self.tool_scrolls.clone();
                            let streaming = self.streaming;
                            let live_tool_group = (self.busy
                                && self
                                    .messages
                                    .last()
                                    .is_some_and(|message| message.author == Author::Tool))
                            .then_some(self.messages.len().saturating_sub(1));
                            let row_context = Rc::new(MessageRowContext {
                                streaming,
                                live_tool_group,
                                cwd,
                                expanded_tools,
                                expanded_tool_groups,
                                tool_scrolls,
                                markdown_selection: self.markdown_selection.clone(),
                            });
                            let panel = cx.entity();
                            content = content.child(
                                div()
                                    .h_full()
                                    .w_full()
                                    .child(self.markdown_selection_input())
                                    .child(md::render::frame_reset(self.markdown_selection.clone()))
                                    .child(
                                        MessageScroller::new(
                                            "ai-messages",
                                            self.message_scroller.clone(),
                                            move |ix, window, cx| {
                                                render_message_row(
                                                    messages.get(ix).cloned(),
                                                    ix,
                                                    &row_context,
                                                    &panel,
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )
                                        .with_row_style(StyleRefinement::default().pb(rems(0.75)))
                                        .with_jump_button_label(t!("ai.jump_to_latest")),
                                    ),
                            );
                        }
                        content
                    }))
                    .child(
                        div().absolute().left_3().right_3().bottom_3().child(
                            v_flex()
                                .w_full()
                                .gap_2()
                                .when_some(
                                    self.permission.clone().zip(self.permission_state.clone()),
                                    |stack, (permission, state)| {
                                        stack.child(self.render_permission(permission, state, cx))
                                    },
                                )
                                .child(self.render_composer(window, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }
}

#[derive(Clone)]
struct ContextMeter {
    progress_percent: f32,
    color: Hsla,
    label: SharedString,
    tooltip: SharedString,
}

fn context_meter(usage: Option<claude::ClaudeContextUsage>, cx: &App) -> ContextMeter {
    let Some(usage) = usage else {
        let label = t!("ai.composer.context_usage_pending");
        return ContextMeter {
            progress_percent: 0.0,
            color: cx.theme().muted_foreground,
            label: label.clone(),
            tooltip: label,
        };
    };
    let used = usage.used_tokens.min(usage.max_tokens);
    let remaining = usage.max_tokens.saturating_sub(used);
    let remaining_percent = if usage.max_tokens == 0 {
        0.0
    } else {
        remaining as f32 / usage.max_tokens as f32 * 100.0
    };
    let remaining_percent = remaining_percent.clamp(0.0, 100.0);
    let color = if remaining_percent <= 10.0 {
        cx.theme().danger
    } else if remaining_percent <= 30.0 {
        cx.theme().warning
    } else {
        cx.theme().primary
    };
    let percent = format!("{remaining_percent:.0}");
    let label = t!("ai.composer.context_usage_label", percent = percent.clone());
    let tooltip = t!(
        "ai.composer.context_usage",
        remaining = format_tokens(remaining),
        max = format_tokens(usage.max_tokens),
        percent = percent,
        used = format_tokens(usage.used_tokens),
        input = format_tokens(usage.input_tokens),
        output = format_tokens(usage.output_tokens),
        cache_read = format_tokens(usage.cache_read_input_tokens),
        cache_write = format_tokens(usage.cache_creation_input_tokens)
    );

    ContextMeter {
        progress_percent: 100.0 - remaining_percent,
        color,
        label,
        tooltip,
    }
}

fn context_progress_action(
    id: &'static str,
    button: impl IntoElement + 'static,
    meter: &ContextMeter,
    cx: &App,
) -> AnyElement {
    let progress = meter.progress_percent.clamp(0.0, 100.0);
    let color = meter.color;
    let track = cx.theme().border;
    let label = meter.label.clone();
    let tooltip = meter.tooltip.clone();
    let size = rems(2.25);
    let radius = cx.theme().radius;
    div()
        .id(id)
        .test_support()
        .aria_label(label.clone())
        .relative()
        .w(size)
        .h(size)
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .child(button)
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    window.paint_path(rounded_rect_progress_path(bounds, radius, 1.0), track);
                    if progress > 0.0 {
                        window.paint_path(
                            rounded_rect_progress_path(bounds, radius, progress / 100.0),
                            color,
                        );
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element()
}

fn rounded_rect_progress_path(
    bounds: Bounds<Pixels>,
    radius: Pixels,
    progress: f32,
) -> gpui_kit::Path<Pixels> {
    const SAMPLES: usize = 192;
    let stroke = px(2.0);
    let left = bounds.left() + stroke / 2.0;
    let top = bounds.top() + stroke / 2.0;
    let right = bounds.right() - stroke / 2.0;
    let bottom = bounds.bottom() - stroke / 2.0;
    let width = (right - left).max(px(0.0));
    let height = (bottom - top).max(px(0.0));
    let radius = radius.min(width.min(height) / 2.0).max(px(0.0));
    let horizontal = (width - radius * 2.0).max(px(0.0));
    let vertical = (height - radius * 2.0).max(px(0.0));
    let arc = radius * std::f32::consts::FRAC_PI_2;
    let lengths = [
        horizontal, arc, vertical, arc, horizontal, arc, vertical, arc,
    ];
    let total = lengths
        .iter()
        .copied()
        .fold(px(0.0), |total, length| total + length);
    let target = total * progress.clamp(0.0, 1.0);
    let point_at = |distance: Pixels| {
        let mut remaining = distance;
        for (index, length) in lengths.iter().copied().enumerate() {
            if remaining > length && length > px(0.0) {
                remaining -= length;
                continue;
            }
            let ratio = if length == px(0.0) {
                0.0
            } else {
                (remaining / length).clamp(0.0, 1.0)
            };
            return match index {
                0 => point(left + radius + horizontal * ratio, top),
                1 => {
                    let angle = -std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2 * ratio;
                    point(
                        right - radius + angle.cos() * radius,
                        top + radius + angle.sin() * radius,
                    )
                }
                2 => point(right, top + radius + vertical * ratio),
                3 => {
                    let angle = std::f32::consts::FRAC_PI_2 * ratio;
                    point(
                        right - radius + angle.cos() * radius,
                        bottom - radius + angle.sin() * radius,
                    )
                }
                4 => point(right - radius - horizontal * ratio, bottom),
                5 => {
                    let angle = std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2 * ratio;
                    point(
                        left + radius + angle.cos() * radius,
                        bottom - radius + angle.sin() * radius,
                    )
                }
                6 => point(left, bottom - radius - vertical * ratio),
                _ => {
                    let angle = std::f32::consts::PI + std::f32::consts::FRAC_PI_2 * ratio;
                    point(
                        left + radius + angle.cos() * radius,
                        top + radius + angle.sin() * radius,
                    )
                }
            };
        }
        point(left + radius, top)
    };

    let mut path = PathBuilder::stroke(stroke);
    path.move_to(point(left + radius, top));
    for sample in 1..=SAMPLES {
        let distance = target * (sample as f32 / SAMPLES as f32);
        path.line_to(point_at(distance));
    }
    if progress >= 0.999 {
        path.close();
    }
    path.build().expect("rounded progress path is valid")
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn render_composer_attachment(
    ix: usize,
    path: &Path,
    cwd: &Path,
    panel: Entity<AiAssistantPanel>,
    cx: &mut Context<AiAssistantPanel>,
) -> AnyElement {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let id = SharedString::from(format!("ai-attachment-{ix}"));
    let remove_id = SharedString::from(format!("ai-attachment-remove-{ix}"));
    let remove_panel = panel.clone();
    let tooltip_name = SharedString::from(name.clone());
    let preview_path = path.to_path_buf();
    let preview_name = name.clone();
    let preview_cwd = cwd.to_path_buf();

    if claude_media_type(path).is_some() {
        div()
            .id(id)
            .test_support()
            .relative()
            .w(rems(4.5))
            .h(rems(4.5))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .tooltip(move |window, cx| Tooltip::new(tooltip_name.clone()).build(window, cx))
            .child(
                img(markdown_image_source(&path.to_string_lossy(), cwd))
                    .id(SharedString::from(format!("ai-attachment-image-{ix}")))
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .on_click(move |_, window, cx| {
                        open_ai_image_preview(
                            &preview_path.to_string_lossy(),
                            &preview_name,
                            &preview_cwd,
                            window,
                            cx,
                        )
                    })
                    .test_support(),
            )
            .child(
                div().absolute().top(rems(0.25)).right(rems(0.25)).child(
                    Button::new(remove_id)
                        .ghost()
                        .xsmall()
                        .rounded_full()
                        .icon(IconName::Close)
                        .tooltip(t!("ai.attachments.remove"))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            // Remove after this click is dispatched: removing the
                            // thumbnail synchronously lets the same pointer event
                            // fall through to the image and open its preview.
                            let remove_panel = remove_panel.clone();
                            window.defer(cx, move |_, cx| {
                                remove_panel
                                    .update(cx, |panel, cx| panel.remove_attachment(ix, cx));
                            });
                        }),
                ),
            )
            .into_any_element()
    } else {
        h_flex()
            .id(id)
            .test_support()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .text_xs()
            .child(name)
            .child(
                Button::new(remove_id)
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t!("ai.attachments.remove"))
                    .on_click(move |_, _, cx| {
                        panel.update(cx, |panel, cx| panel.remove_attachment(ix, cx));
                    }),
            )
            .into_any_element()
    }
}

fn paste_composer_attachments(
    panel: &Entity<AiAssistantPanel>,
    item: &ClipboardItem,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let mut handled = false;
    let mut paths = Vec::new();
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Image(image) => {
                handled = true;
                match save_pasted_image(image) {
                    Ok(path) => paths.push(path),
                    Err(error) => {
                        window.push_notification(Notification::error(error), cx);
                    }
                }
            }
            ClipboardEntry::ExternalPaths(external) => {
                handled = true;
                paths.extend(external.paths().iter().cloned());
            }
            ClipboardEntry::String(_) => {}
        }
    }
    if !paths.is_empty() {
        panel.update(cx, |panel, cx| {
            for path in paths {
                if !panel.attachments.contains(&path) {
                    panel.attachments.push(path);
                }
            }
            cx.notify();
        });
    }
    handled
}

fn save_pasted_image(image: &Image) -> Result<PathBuf, SharedString> {
    let directory = std::env::temp_dir().join("shellrs-ai-pasted-images");
    std::fs::create_dir_all(&directory)
        .map_err(|error| t!("ai.attachments.paste_failed", error = error.to_string()))?;
    let path = directory.join(format!(
        "pasted-{}.{}",
        Uuid::new_v4(),
        image.format.extension()
    ));
    std::fs::write(&path, &image.bytes)
        .map_err(|error| t!("ai.attachments.paste_failed", error = error.to_string()))?;
    Ok(path)
}

fn active_environment(ai: &AiAssistantSettings) -> Option<&crate::settings::AiEnvironment> {
    ai.active_environment
        .and_then(|id| {
            ai.environments
                .iter()
                .find(|environment| environment.id == id)
        })
        .or_else(|| ai.environments.first())
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "tif"
                    | "tiff"
                    | "ico"
                    | "svg"
                    | "pbm"
                    | "ppm"
                    | "pgm"
                    | "pnm"
            )
        })
}

fn claude_media_type(path: &Path) -> Option<MediaType> {
    match image_format_for_path(path)? {
        ImageFormat::Png => Some(MediaType::Png),
        ImageFormat::Jpeg => Some(MediaType::Jpeg),
        ImageFormat::Gif => Some(MediaType::Gif),
        ImageFormat::Webp => Some(MediaType::Webp),
        _ => None,
    }
}

fn claude_image_data(path: &Path) -> Result<String, SharedString> {
    const LIMIT: u64 = 10 * 1024 * 1024;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let size = std::fs::metadata(path)
        .map_err(|error| {
            t!(
                "ai.attachments.unreadable",
                name = name,
                error = error.to_string()
            )
        })?
        .len();
    if size > LIMIT {
        return Err(t!("ai.attachments.image_too_large", name = name));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        t!(
            "ai.attachments.unreadable",
            name = name,
            error = error.to_string()
        )
    })?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn read_attachment(path: &PathBuf) -> Result<String, SharedString> {
    const LIMIT: u64 = 512 * 1024;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let size = std::fs::metadata(path)
        .map_err(|error| {
            t!(
                "ai.attachments.unreadable",
                name = name,
                error = error.to_string()
            )
        })?
        .len();
    if size > LIMIT {
        return Err(t!("ai.attachments.too_large", name = name));
    }
    std::fs::read_to_string(path).map_err(|error| {
        t!(
            "ai.attachments.unreadable",
            name = name,
            error = error.to_string()
        )
    })
}

static MARKDOWN_IMAGES: OnceLock<Mutex<HashMap<String, Arc<Image>>>> = OnceLock::new();

fn open_ai_image_preview(url: &str, alt: &str, cwd: &Path, window: &mut Window, cx: &mut App) {
    let bytes = local_image_path(url, cwd)
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len());
    let Some(image) = decode_data_image(url).or_else(|| read_local_image(url, cwd)) else {
        let name = image_preview_name(url, alt, cwd);
        window.push_notification(
            Notification::error(t!("ai.image_preview.failed", name = name)),
            cx,
        );
        return;
    };
    let name = image_preview_name(url, alt, cwd);
    open_image_preview(image, name, bytes, window, cx);
}

fn image_preview_name(url: &str, alt: &str, cwd: &Path) -> SharedString {
    if !alt.trim().is_empty() {
        return alt.trim().to_owned().into();
    }
    if let Some(path) = local_image_path(url, cwd)
        && let Some(name) = path.file_name()
    {
        return name.to_string_lossy().into_owned().into();
    }
    if let Ok(parsed) = url::Url::parse(url)
        && let Some(name) = parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|name| !name.is_empty())
    {
        return name.to_owned().into();
    }
    t!("ai.image_preview.title")
}

fn markdown_image_source(url: &str, cwd: &Path) -> ImageSource {
    let url = url.to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.into();
    }
    let cache = MARKDOWN_IMAGES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(images) = cache.lock()
        && let Some(image) = images.get(&url)
    {
        return ImageSource::Image(image.clone());
    }
    let image = decode_data_image(&url).or_else(|| read_local_image(&url, cwd));
    if let Some(image) = image {
        let image = Arc::new(image);
        if let Ok(mut images) = cache.lock() {
            images.insert(url, image.clone());
        }
        return ImageSource::Image(image);
    }
    url.into()
}

fn decode_data_image(url: &str) -> Option<Image> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let base64 = meta.ends_with(";base64");
    let mime = meta.strip_suffix(";base64").unwrap_or(meta);
    let format = ImageFormat::from_mime_type(mime)?;
    let bytes = if base64 {
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .ok()?
    } else {
        data.as_bytes().to_vec()
    };
    Some(Image::from_bytes(format, bytes))
}

fn read_local_image(url: &str, cwd: &Path) -> Option<Image> {
    let path = local_image_path(url, cwd)?;
    read_ai_image_file(&path)
}

pub(crate) fn read_ai_image_file(path: &Path) -> Option<Image> {
    let bytes = std::fs::read(path).ok()?;
    let format = image_format_for_path(path).or_else(|| sniff_image_format(&bytes))?;
    Some(Image::from_bytes(format, bytes))
}

fn local_image_path(url: &str, cwd: &Path) -> Option<PathBuf> {
    if let Ok(parsed) = url::Url::parse(url) {
        if parsed.scheme() != "file" {
            return None;
        }
        return parsed.to_file_path().ok();
    }
    if let Some(rest) = url.strip_prefix("~/") {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    let path = PathBuf::from(url);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn image_format_for_path(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        "pbm" | "ppm" | "pgm" | "pnm" => ImageFormat::Pnm,
        _ => return None,
    })
}

fn sniff_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(b"\xff\xd8") {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if bytes.trim_ascii_start().starts_with(b"<svg")
        || bytes.trim_ascii_start().starts_with(b"<?xml")
    {
        Some(ImageFormat::Svg)
    } else {
        None
    }
}

fn confirmation_menu(
    panel: Entity<AiAssistantPanel>,
    settings: Entity<SettingsStore>,
    selected: AiConfirmationStrategy,
) -> AnyElement {
    Button::new("ai-confirmation")
        .ghost()
        .xsmall()
        .icon(Icon::new(confirmation_icon(selected)))
        .label(selected.label())
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
            AiConfirmationStrategy::ALL
                .iter()
                .fold(menu, |menu, &strategy| {
                    let settings = settings.clone();
                    let panel = panel.clone();
                    menu.item(
                        PopupMenuItem::new(strategy.label())
                            .icon(Icon::new(confirmation_icon(strategy)))
                            .checked(strategy == selected)
                            .on_click(move |_, _, cx| {
                                settings.update(cx, |store, cx| {
                                    store.update(|settings| settings.ai.confirmation = strategy, cx)
                                });
                                panel.update(cx, |panel, cx| panel.reset_session(cx));
                            }),
                    )
                })
        })
        .into_any_element()
}

fn confirmation_icon(strategy: AiConfirmationStrategy) -> CatalogIcon {
    match strategy {
        AiConfirmationStrategy::Balanced => CatalogIcon::ShieldCheck,
        AiConfirmationStrategy::AlwaysAsk => CatalogIcon::MessageSquarePlus,
        AiConfirmationStrategy::AutoApprove => CatalogIcon::Zap,
    }
}

fn model_menu(
    panel: Entity<AiAssistantPanel>,
    selected: Option<ClaudeModel>,
    label: SharedString,
) -> AnyElement {
    let default_panel = panel.clone();
    let default_checked = selected.is_none();
    let models = claude_models();
    Button::new("ai-model")
        .ghost()
        .xsmall()
        .label(label)
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let default_panel = default_panel.clone();
            let models = models.clone();
            let menu = menu.item(
                PopupMenuItem::new(t!("ai.model.default"))
                    .checked(default_checked)
                    .on_click(move |_, _, cx| {
                        default_panel.update(cx, |panel, cx| {
                            panel.model = None;
                            panel.reset_session(cx);
                        });
                    }),
            );
            models.into_iter().fold(menu.separator(), |menu, model| {
                let panel = panel.clone();
                let checked = selected.as_ref() == Some(&model);
                menu.item(
                    PopupMenuItem::new(SharedString::from(model.display_name().to_string()))
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            let model = model.clone();
                            panel.update(cx, |panel, cx| {
                                panel.model = Some(model);
                                panel.reset_session(cx);
                            });
                        }),
                )
            })
        })
        .into_any_element()
}

fn claude_models() -> Vec<ClaudeModel> {
    vec![
        ClaudeModel::Sonnet,
        ClaudeModel::Opus,
        ClaudeModel::Haiku,
        ClaudeModel::Fable,
        ClaudeModel::Best,
        ClaudeModel::OpusPlan,
        ClaudeModel::Sonnet1m,
        ClaudeModel::Opus1m,
        ClaudeModel::Fable1m,
    ]
}

struct AiHistoryDialog {
    sessions: Vec<history::ConversationSummary>,
    cwd: PathBuf,
    list: Entity<ListState<HistoryList>>,
    query: Entity<InputState>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

struct HistoryList {
    panel: Entity<AiAssistantPanel>,
    sessions: Vec<history::ConversationSummary>,
    filtered: Vec<history::ConversationSummary>,
    current: Option<Uuid>,
    selected: Option<usize>,
}

impl HistoryList {
    fn new(
        panel: Entity<AiAssistantPanel>,
        sessions: Vec<history::ConversationSummary>,
        current: Option<Uuid>,
    ) -> Self {
        Self {
            panel,
            filtered: sessions.clone(),
            sessions,
            current,
            selected: None,
        }
    }

    fn filter(&mut self, query: &str) {
        let query = query.to_lowercase();
        self.filtered = self
            .sessions
            .iter()
            .filter(|session| {
                query.is_empty()
                    || session.title.to_lowercase().contains(&query)
                    || session
                        .model
                        .as_deref()
                        .is_some_and(|model| model.to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        self.selected = None;
    }

    fn open(&mut self, index: usize, window: &mut Window, cx: &mut App) {
        let Some(session) = self.filtered.get(index).cloned() else {
            return;
        };
        if let Some(conversation) = history::load(&session) {
            self.panel
                .update(cx, |panel, cx| panel.adopt_history(conversation, cx));
            window.close_dialog(cx);
        }
    }
}

impl ListDelegate for HistoryList {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.filter(query);
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.filtered.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let session = self.filtered.get(ix.row)?.clone();
        let current = self.current == Some(session.id);
        let time = chrono::DateTime::<chrono::Local>::from(session.updated)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let model = session.model.clone().unwrap_or_else(|| "Claude".into());
        let muted = cx.theme().muted_foreground;
        let panel = self.panel.clone();
        let summary = session.clone();
        Some(
            ListItem::new(ElementId::Name(format!("ai-history:{}", session.id).into()))
                .w_full()
                .h(rems(3.5))
                .px_3()
                .rounded(cx.theme().radius)
                .child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .truncate()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(session.title.clone()),
                                )
                                .when(current, |row| {
                                    row.child(
                                        Tag::primary()
                                            .small()
                                            .rounded_full()
                                            .child(t!("ai.history.current")),
                                    )
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .text_color(muted)
                                .child(time)
                                .child(t!("ai.history.turns", count = session.turns))
                                .child(model),
                        ),
                )
                .on_click(move |_, window, cx| {
                    if let Some(conversation) = history::load(&summary) {
                        panel.update(cx, |panel, cx| panel.adopt_history(conversation, cx));
                        window.close_dialog(cx);
                    }
                }),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|ix| ix.row);
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        if let Some(selected) = self.selected {
            self.open(selected, window, cx);
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(t!("ai.history.empty_list"))
    }
}

impl AiHistoryDialog {
    fn new(
        panel: Entity<AiAssistantPanel>,
        sessions: Vec<history::ConversationSummary>,
        cwd: PathBuf,
        current: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t!("ai.history.search")));
        let list = cx.new(|cx| {
            ListState::new(
                HistoryList::new(panel.clone(), sessions.clone(), current),
                window,
                cx,
            )
        });
        let query_reader = query.clone();
        let subscriptions = vec![cx.subscribe_in(&query, window, {
            let list = list.clone();
            move |_, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = query_reader.read(cx).value().to_string();
                    list.update(cx, |list, cx| {
                        list.delegate_mut().filter(&query);
                        list.set_selected_index(None, window, cx);
                    });
                }
            }
        })];
        Self {
            sessions,
            cwd,
            list,
            query,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn clear_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let dialog = cx.entity();
        crate::shared::confirm_danger(
            t!("ai.history.clear_title"),
            Some(t!("ai.history.clear_description")),
            t!("ai.history.clear"),
            std::rc::Rc::new(move |window, cx| {
                let removed = history::clear(&cwd).unwrap_or_default();
                dialog.update(cx, |dialog, cx| {
                    dialog.sessions.clear();
                    let list = dialog.list.clone();
                    list.update(cx, |list, cx| {
                        list.delegate_mut().sessions.clear();
                        list.delegate_mut().filtered.clear();
                        list.delegate_mut().selected = None;
                        cx.notify();
                    });
                    cx.notify();
                });
                window.push_notification(
                    Notification::success(t!("ai.history.cleared", count = removed)),
                    cx,
                );
            }),
            window,
            cx,
        );
    }
}

impl Focusable for AiHistoryDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AiHistoryDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let clear = Button::new("ai-history-clear")
            .ghost()
            .xsmall()
            .label(t!("ai.history.clear"))
            .tooltip(t!("ai.history.clear_tooltip"))
            .disabled(self.sessions.is_empty())
            .on_click(cx.listener(|this, _, window, cx| this.clear_history(window, cx)));
        let close = Button::new("ai-history-close")
            .ghost()
            .xsmall()
            .icon(IconName::Close)
            .tooltip(t!("common.close"))
            .on_click(|_, window, cx| window.close_dialog(cx));
        let list = self.list.clone();

        v_flex()
            .id("ai-history")
            .test_support()
            .w_full()
            .max_h(rems(28.))
            .gap_3()
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(move |_this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "up" | "down" => {
                        let len = list.read(cx).delegate().filtered.len();
                        if len == 0 {
                            return;
                        }
                        let current = list.read(cx).selected_index().map_or(
                            if event.keystroke.key == "down" {
                                0
                            } else {
                                len - 1
                            },
                            |ix| {
                                if event.keystroke.key == "down" {
                                    (ix.row + 1).min(len - 1)
                                } else {
                                    ix.row.saturating_sub(1)
                                }
                            },
                        );
                        list.update(cx, |list, cx| {
                            list.set_selected_index(Some(IndexPath::new(current)), window, cx);
                            list.scroll_to_item(
                                IndexPath::new(current),
                                ScrollStrategy::Nearest,
                                window,
                                cx,
                            );
                        });
                    }
                    "enter" => {
                        list.update(cx, |list, cx| {
                            list.delegate_mut().confirm(false, window, cx)
                        });
                    }
                    "escape" => window.close_dialog(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_start()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t!("ai.history.title")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(t!("ai.history.description")),
                            ),
                    )
                    .child(h_flex().items_center().gap_1().child(clear).child(close)),
            )
            .child(
                Input::new(&self.query)
                    .id("ai-history-search")
                    .prefix(IconName::Search)
                    .cleanable(true)
                    .small(),
            )
            .child(
                div()
                    .w_full()
                    .max_h(rems(18.))
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .overflow_hidden()
                    .child(List::new(&self.list).max_h(rems(18.))),
            )
    }
}

/// Open 历史对话 for the AI panel's current local working directory.
pub(crate) fn open_history_dialog(
    panel: Entity<AiAssistantPanel>,
    window: &mut Window,
    cx: &mut App,
) {
    let (cwd, current) = {
        let panel = panel.read(cx);
        (panel.current_cwd(cx), panel.current_session())
    };
    let sessions = history::sessions(&cwd);
    let dialog =
        cx.new(|cx| AiHistoryDialog::new(panel.clone(), sessions, cwd, current, window, cx));
    window.open_dialog(cx, {
        let dialog = dialog.clone();
        move |surface, window, _cx| {
            surface
                .close_button(false)
                .w(rems(32.).to_pixels(window.rem_size()))
                .overlay_closable(true)
                .keyboard(true)
                .child(dialog.clone())
        }
    });
    let query = dialog.read(cx).query.clone();
    query.update(cx, |input, cx| input.focus(window, cx));
}

#[cfg(test)]
mod tests {
    use super::{
        AiAssistantPanel, Author, ClaudeEvent, Message, ToolCall, ToolStatus, append_tool_call,
        claude_media_type, coalesce_claude_events, decode_data_image, format_tokens,
        image_format_for_path, is_image_path, sniff_image_format, streaming_tail, tool_file_change,
        tool_group_title, tool_input_text, tool_summary, tool_title_key,
    };
    use claude_codes::MediaType;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Context, Entity, Focusable as _, ImageFormat, IntoElement, ParentElement,
        Render, Styled, Window, div,
    };
    use std::{cell::RefCell, path::Path, rc::Rc};

    #[test]
    fn token_counts_are_compact_for_the_context_tooltip() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(200_000), "200.0K");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn streamed_text_chunks_coalesce_without_crossing_other_events() {
        let events = coalesce_claude_events(vec![
            ClaudeEvent::Text("one ".into()),
            ClaudeEvent::Text("two".into()),
            ClaudeEvent::Done,
            ClaudeEvent::Text("three".into()),
        ]);
        assert!(matches!(&events[0], ClaudeEvent::Text(text) if text == "one two"));
        assert!(matches!(&events[1], ClaudeEvent::Done));
        assert!(matches!(&events[2], ClaudeEvent::Text(text) if text == "three"));
    }

    #[test]
    fn streaming_tail_keeps_only_the_last_line_and_characters() {
        assert_eq!(streaming_tail("").as_deref(), None);
        assert_eq!(streaming_tail("first\nsecond").as_deref(), Some("second"));
        let long = format!("prefix {}", "字".repeat(60));
        let tail = streaming_tail(&long).unwrap();
        assert!(tail.starts_with('…'));
        assert_eq!(tail.chars().count(), 49);
        assert!(tail.ends_with("字"));
    }

    #[test]
    fn image_formats_are_read_from_extensions_and_bytes() {
        assert_eq!(
            image_format_for_path(Path::new("shot.PNG")),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            sniff_image_format(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageFormat::Png)
        );
        assert_eq!(sniff_image_format(b"not an image"), None);
    }

    #[test]
    fn claude_image_attachments_keep_their_media_type() {
        assert!(is_image_path(Path::new("shot.JPG")));
        assert_eq!(
            claude_media_type(Path::new("shot.jpg")),
            Some(MediaType::Jpeg)
        );
        assert_eq!(
            claude_media_type(Path::new("shot.png")),
            Some(MediaType::Png)
        );
        assert_eq!(claude_media_type(Path::new("notes.txt")), None);
    }

    #[test]
    fn base64_data_images_decode() {
        let image = decode_data_image("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(image.format, ImageFormat::Png);
        assert_eq!(image.bytes, b"\x89PNG\r\n\x1a\n");
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            summary: "target".into(),
            input_text: "input".into(),
            output: None,
            status: ToolStatus::Running,
            file_change: None,
            diff_state: None,
        }
    }

    #[test]
    fn adjacent_tool_calls_share_one_activity_group() {
        let mut messages = Vec::new();
        append_tool_call(&mut messages, tool_call("first", "Bash"));
        append_tool_call(&mut messages, tool_call("second", "Read"));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].author, Author::Tool);
        assert_eq!(messages[0].tools.len(), 2);

        messages.push(Message {
            author: Author::Assistant,
            text: "between".into(),
            time: None,
            images: Vec::new(),
            queue_position: None,
            markdown_view: None,
            thinking_tail: "".into(),
            tools: Vec::new(),
        });
        append_tool_call(&mut messages, tool_call("third", "Edit"));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].tools.len(), 1);
    }

    #[test]
    fn claude_edit_inputs_become_parseable_diffs() {
        let input = serde_json::json!({
            "file_path": "/tmp/example.rs",
            "old_string": "fn old() {\n    one();\n}",
            "new_string": "fn renamed() {\n    one();\n    two();\n}"
        });
        let change = tool_file_change("Edit", &input).unwrap();
        let files = gpui_kit::component::diff::DiffFile::parse(&change.patch).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path().as_ref(), "/tmp/example.rs");
        assert_eq!(files[0].additions(), 3);
        assert_eq!(files[0].deletions(), 2);
        assert_eq!(change.additions, 3);
        assert_eq!(change.deletions, 2);
    }

    #[test]
    fn claude_write_and_multi_edit_inputs_become_diffs() {
        let write = serde_json::json!({
            "file_path": "/tmp/new.txt",
            "content": "first\nsecond"
        });
        let change = tool_file_change("Write", &write).unwrap();
        let files = gpui_kit::component::diff::DiffFile::parse(&change.patch).unwrap();
        assert_eq!(files[0].additions(), 2);
        assert_eq!(files[0].deletions(), 0);

        let multi = serde_json::json!({
            "file_path": "/tmp/multi.rs",
            "edits": [
                {"old_string": "one", "new_string": "ONE"},
                {"old_string": "two", "new_string": "TWO"}
            ]
        });
        let change = tool_file_change("MultiEdit", &multi).unwrap();
        let files = gpui_kit::component::diff::DiffFile::parse(&change.patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].additions(), 2);
        assert_eq!(files[0].deletions(), 2);
    }

    #[test]
    fn settled_tool_groups_use_kind_counts() {
        let mut bash = tool_call("one", "Bash");
        bash.status = ToolStatus::Succeeded;
        let mut read = tool_call("two", "Read");
        read.status = ToolStatus::Succeeded;
        assert_eq!(
            tool_group_title(&[bash, read], false).as_ref(),
            "已执行：命令 × 1 · 读取 × 1"
        );
    }

    #[test]
    fn tool_cards_use_compact_titles_and_summaries() {
        let bash = serde_json::json!({"command": "shellrs exec web-01 'uptime && df -h'"});
        assert_eq!(tool_title_key("Bash"), "ai.tool.title.command");
        assert_eq!(tool_summary("Bash", &bash), bash["command"]);
        assert_eq!(tool_input_text("Bash", &bash), bash["command"]);

        let read = serde_json::json!({"file_path": "/etc/nginx/nginx.conf"});
        assert_eq!(tool_title_key("Read"), "ai.tool.title.read");
        assert_eq!(tool_summary("Read", &read), "/etc/nginx/nginx.conf");

        let unknown = serde_json::json!({"query": "disk usage"});
        assert_eq!(tool_title_key("custom_tool"), "ai.tool.title.generic");
        assert_eq!(
            tool_summary("custom_tool", &unknown),
            "custom_tool · disk usage"
        );
    }

    struct ComposerFocusHarness(Entity<AiAssistantPanel>);

    impl Render for ComposerFocusHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(gpui_kit::px(280.))
                .h(gpui_kit::px(600.))
                .relative()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .left_3()
                        .right_3()
                        .bottom_3()
                        .child(self.0.clone()),
                )
        }
    }

    #[gpui_kit::test]
    fn clicking_the_upper_composer_focuses_the_textarea(cx: &mut gpui_kit::TestAppContext) {
        cx.update(crate::init);
        let slot = Rc::new(RefCell::new(None));
        let panel_slot = slot.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let settings = cx.new(|_| crate::settings::SettingsStore::in_memory());
            let panel = cx.new(|cx| AiAssistantPanel::new(settings, window, cx));
            panel.update(cx, |panel, cx| {
                panel.set_target(Some("web-01".into()), true, cx);
                panel.input.update(cx, |input, cx| {
                    let source = (0..20)
                        .map(|i| format!("line {i}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    input.set_value(source, window, cx);
                });
            });
            *panel_slot.borrow_mut() = Some(panel.clone());
            ComposerFocusHarness(panel)
        });
        let panel = slot.borrow_mut().take().unwrap();
        let draw = |cx: &mut gpui_kit::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        for _ in 0..5 {
            draw(cx);
        }
        let composer = cx.update(|window, _| window.find("ai-composer").bounds());
        let at = gpui_kit::point(composer.center().x, composer.top() + gpui_kit::px(16.));
        cx.simulate_click(at, Default::default());
        let focused = cx.update(|window, cx| {
            panel
                .read(cx)
                .input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        });
        assert!(
            focused,
            "clicking the upper half of {composer:?} should focus the textarea"
        );
    }

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui_kit::test]
    fn switching_targets_keeps_the_current_conversation(cx: &mut gpui_kit::TestAppContext) {
        cx.update(crate::init);
        let slot = Rc::new(RefCell::new(None));
        let panel_slot = slot.clone();
        let _ = cx.add_window_view(move |window, cx| {
            let settings = cx.new(|_| crate::settings::SettingsStore::in_memory());
            let panel = cx.new(|cx| AiAssistantPanel::new(settings, window, cx));
            *panel_slot.borrow_mut() = Some(panel);
            Probe
        });
        let panel = slot.borrow_mut().take().unwrap();
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                panel.set_target(Some("web-01".into()), true, cx);
                panel.messages_mut().push(Message {
                    author: Author::Notice,
                    text: "keep this conversation".into(),
                    time: None,
                    images: Vec::new(),
                    queue_position: None,
                    markdown_view: None,
                    thinking_tail: "".into(),
                    tools: Vec::new(),
                });
                panel.busy = true;
                panel.pending_turns = 1;

                panel.set_target(Some("db-01".into()), false, cx);

                assert_eq!(panel.messages.len(), 1);
                assert_eq!(panel.messages[0].text.as_str(), "keep this conversation");
                assert!(panel.busy);
                assert_eq!(panel.pending_turns, 1);
            });
        });
    }

    #[gpui_kit::test]
    fn queued_messages_advance_to_the_front(cx: &mut gpui_kit::TestAppContext) {
        cx.update(crate::init);
        let slot = Rc::new(RefCell::new(None));
        let panel_slot = slot.clone();
        let _ = cx.add_window_view(move |window, cx| {
            let settings = cx.new(|_| crate::settings::SettingsStore::in_memory());
            let panel = cx.new(|cx| AiAssistantPanel::new(settings, window, cx));
            *panel_slot.borrow_mut() = Some(panel);
            Probe
        });
        let panel = slot.borrow_mut().take().unwrap();
        cx.update(|cx| {
            panel.update(cx, |panel, _| {
                for (queue_position, text) in
                    [(Some(1), "first"), (Some(2), "second"), (Some(3), "third")]
                {
                    panel.messages_mut().push(Message {
                        author: Author::User,
                        text: text.into(),
                        time: None,
                        images: Vec::new(),
                        queue_position,
                        markdown_view: None,
                        thinking_tail: "".into(),
                        tools: Vec::new(),
                    });
                }

                panel.mark_next_queued_message_sent();
                let positions: Vec<_> = panel
                    .messages
                    .iter()
                    .map(|message| message.queue_position)
                    .collect();
                assert_eq!(positions, vec![None, Some(1), Some(2)]);

                panel.clear_queued_messages();
                assert!(
                    panel
                        .messages
                        .iter()
                        .all(|message| message.queue_position.is_none())
                );
            });
        });
    }

    #[gpui_kit::test]
    fn queued_message_actions_update_the_queue(cx: &mut gpui_kit::TestAppContext) {
        cx.update(crate::init);
        let slot = Rc::new(RefCell::new(None));
        let panel_slot = slot.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let settings = cx.new(|_| crate::settings::SettingsStore::in_memory());
            let panel = cx.new(|cx| AiAssistantPanel::new(settings, window, cx));
            *panel_slot.borrow_mut() = Some(panel);
            Probe
        });
        let panel = slot.borrow_mut().take().unwrap();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                for (text, position) in [("first", 1), ("second", 2)] {
                    panel.queued_prompts.push_back(crate::claude::ClaudePrompt {
                        text: text.into(),
                        images: Vec::new(),
                    });
                    panel.messages_mut().push(Message {
                        author: Author::User,
                        text: text.into(),
                        time: None,
                        images: Vec::new(),
                        queue_position: Some(position),
                        markdown_view: None,
                        thinking_tail: "".into(),
                        tools: Vec::new(),
                    });
                }
                panel.pending_turns = 3;
                panel.busy = true;

                panel.edit_queued_message(1, window, cx);
                assert_eq!(panel.input.read(cx).value().as_ref(), "first");
                assert_eq!(panel.pending_turns, 2);
                assert_eq!(panel.messages.len(), 1);
                assert_eq!(panel.messages[0].queue_position, Some(1));
                assert_eq!(panel.queued_prompts[0].text, "second");
            });
        });
    }
}
