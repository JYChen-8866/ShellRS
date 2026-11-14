//! 设置: the settings tab, the settings it edits and how they are applied.

mod app_theme;
mod apply;
mod background_images;
mod highlight_rules;
mod local_shell_editor;
mod model;
mod settings_panel;
mod shortcuts_editor;
mod sidebar_tools;
mod store;
mod sync_page;
mod terminal_themes;

pub use apply::apply;
pub use model::{
    AI_COMMAND_TIMEOUT_RANGE, AI_MAX_ROUNDS_RANGE, AI_PROMPT_LIMIT, AI_RECENT_OUTPUT_RANGE,
    AiAssistantSettings, AiCommandAction, AiCommandRule, AiConfirmationStrategy, AiDisplayLevel,
    AiEnvironment, AiExecutionMode, AiNetworkAccess, AiProtocol, AnalyticsSettings, AppSettings,
    Appearance, BackgroundImageSettings, Choice, DEFAULT_BACKGROUND_OPACITY,
    DEFAULT_INTERFACE_FONT_SIZE, ExternalCliSettings, INTERFACE_FONT_SIZE_RANGE, InterfaceLanguage,
    InterfaceSettings, LocalTerminalSettings, NotificationSettings, SidebarTools, SidebarWidths,
    SyncLocation, SyncSettings, TerminalBackgroundSettings, TerminalFontSettings,
    TerminalHighlightSettings, TerminalThemeSettings, UpdateSettings, WindowSettings,
};
pub use settings_panel::{SettingsPanel, SettingsPanelEvent};
pub use store::{SettingsProblem, SettingsStore, SettingsStoreEvent};
