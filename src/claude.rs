//! Claude Code through its local CLI's JSON-lines protocol. There is no ACP
//! adapter to install. Unix starts the installed `claude` executable directly;
//! Windows runs npm's `claude.cmd` shim through `cmd.exe` when no native
//! `claude.exe` is available.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use async_channel::{Receiver, Sender};
use claude_codes::{
    AsyncClient, ClaudeCliBuilder, ClaudeInput, ClaudeModel, ClaudeOutput, ContentBlock,
    ControlRequestPayload, ImageBlock, ImageSource, ImageSourceType, MediaType, PermissionMode,
    TextBlock, ToolResultContent,
};
use uuid::Uuid;

use crate::i18n::t;
use crate::settings::AiConfirmationStrategy;

/// One prompt, including the images Claude must see as images rather than
/// text. The data is already base64, as the CLI's content block expects.
#[derive(Clone, Debug)]
pub struct ClaudePrompt {
    pub text: String,
    pub images: Vec<(MediaType, String)>,
}

/// Token occupancy and window information from the last completed Claude turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClaudeContextUsage {
    pub used_tokens: u64,
    pub max_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

/// What the Claude process tells the panel while it runs.
#[derive(Clone, Debug)]
pub enum ClaudeEvent {
    Connecting,
    Ready,
    Thinking(String),
    Text(String),
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Permission {
        request_id: String,
        tool_name: String,
        input: serde_json::Value,
        reason: Option<String>,
    },
    PermissionCancelled {
        request_id: String,
    },
    ContextUsage(ClaudeContextUsage),
    Done,
    Error(String),
}

/// The user's answer to one pending tool permission request.
#[derive(Clone, Debug)]
pub enum PermissionDecision {
    Allow,
    Deny(String),
}

/// Control messages that can arrive while a response is streaming.
#[derive(Clone, Copy, Debug)]
pub enum ClaudeCommand {
    Interrupt,
}

/// The local Claude CLI: `SHELLRS_CLAUDE` overrides the automatic lookup.
pub fn claude_command() -> PathBuf {
    static COMMAND: OnceLock<PathBuf> = OnceLock::new();
    COMMAND
        .get_or_init(|| {
            if let Ok(command) = std::env::var("SHELLRS_CLAUDE")
                && !command.trim().is_empty()
            {
                return PathBuf::from(command);
            }
            find_in_paths(&claude_search_paths(), "claude")
                .unwrap_or_else(|| PathBuf::from("claude"))
        })
        .clone()
}

/// PATH for the Claude process itself. GUI apps opened from Finder inherit a
/// minimal PATH, which breaks both locating Claude and the `env node` shebang
/// used by npm and Homebrew installations.
fn claude_process_path() -> OsString {
    std::env::join_paths(claude_search_paths()).unwrap_or_else(|_| {
        std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"))
    })
}

fn claude_search_paths() -> Vec<PathBuf> {
    let mut paths = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let home = dirs::home_dir();

    if let Some(home) = &home {
        paths.extend([
            home.join(".local/bin"),
            home.join(".npm-global/bin"),
            home.join(".volta/bin"),
            home.join(".bun/bin"),
            home.join(".asdf/shims"),
            home.join(".local/share/mise/shims"),
        ]);
        let nvm = home.join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(nvm) {
            let mut versions = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("bin"))
                .collect::<Vec<_>>();
            versions.sort_by(|left, right| right.cmp(left));
            paths.extend(versions);
        }
    }

    #[cfg(windows)]
    {
        if let Some(app_data) = dirs::config_dir() {
            paths.push(app_data.join("npm"));
        }
        if let Some(local_app_data) = dirs::data_local_dir() {
            paths.push(local_app_data.join("Programs").join("claude"));
        }
    }

    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]);

    let mut unique = Vec::with_capacity(paths.len());
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

fn find_in_paths(paths: &[PathBuf], name: &str) -> Option<PathBuf> {
    paths.iter().find_map(|directory| {
        if cfg!(windows) {
            ["exe", "cmd", "bat"].iter().find_map(|extension| {
                let candidate = directory.join(format!("{name}.{extension}"));
                candidate.is_file().then_some(candidate)
            })
        } else {
            let candidate = directory.join(name);
            candidate.is_file().then_some(candidate)
        }
    })
}

#[cfg(windows)]
fn windows_batch_command(command: tokio::process::Command) -> tokio::process::Command {
    use std::os::windows::process::CommandExt as _;

    // CreateProcess cannot execute npm's `.cmd` shims directly. The extensionless
    // `claude` file in the same folder is a Unix shell script and fails with
    // ERROR_BAD_EXE_FORMAT (193), so both lookup order and this wrapper matter.
    let program = command.as_std().get_program().to_os_string();
    let extension = Path::new(&program)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(extension.as_deref(), None | Some("cmd") | Some("bat")) {
        return command;
    }

    let args = command
        .as_std()
        .get_args()
        .map(|arg| arg.to_os_string())
        .collect::<Vec<_>>();
    let current_dir = command.as_std().get_current_dir().map(Path::to_path_buf);
    let envs = command
        .as_std()
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(|value| value.to_os_string())))
        .collect::<Vec<_>>();
    let command_line = windows_batch_command_line(&program, &args);
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
    let mut wrapped = tokio::process::Command::new(shell);
    wrapped
        .arg("/D")
        .arg("/S")
        .arg("/C")
        .raw_arg(command_line)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    if let Some(current_dir) = current_dir {
        wrapped.current_dir(current_dir);
    }
    for (key, value) in envs {
        match value {
            Some(value) => {
                wrapped.env(key, value);
            }
            None => {
                wrapped.env_remove(key);
            }
        }
    }
    wrapped
}

#[cfg(windows)]
fn windows_batch_command_line(program: &std::ffi::OsStr, args: &[OsString]) -> OsString {
    fn quote(arg: &std::ffi::OsStr) -> String {
        let arg = arg.to_string_lossy();
        format!("\"{}\"", arg.replace('"', "\"\""))
    }

    let mut line = String::from("\"");
    line.push_str(&quote(program));
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    line.push('"');
    line.into()
}

/// Run one Claude CLI session on a worker thread, with its own small Tokio
/// runtime. The panel owns the prompt channel; closing it ends the session.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    command: PathBuf,
    cwd: PathBuf,
    model: Option<ClaudeModel>,
    background: bool,
    confirmation: AiConfirmationStrategy,
    session_id: Uuid,
    resume: bool,
    prompts: Receiver<ClaudePrompt>,
    commands: Receiver<ClaudeCommand>,
    permissions: Receiver<PermissionDecision>,
    events: Sender<ClaudeEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("shellrs-claude".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = events.send_blocking(ClaudeEvent::Error(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                if let Err(error) = run(
                    command,
                    cwd,
                    model,
                    background,
                    confirmation,
                    session_id,
                    resume,
                    prompts,
                    commands,
                    permissions,
                    events.clone(),
                )
                .await
                {
                    let _ = events.send(ClaudeEvent::Error(error)).await;
                }
            });
        })
        .expect("start the Claude worker")
}

#[allow(clippy::too_many_arguments)]
async fn run(
    command: PathBuf,
    cwd: PathBuf,
    model: Option<ClaudeModel>,
    background: bool,
    confirmation: AiConfirmationStrategy,
    session_id: Uuid,
    resume: bool,
    prompts: Receiver<ClaudePrompt>,
    commands: Receiver<ClaudeCommand>,
    permissions: Receiver<PermissionDecision>,
    events: Sender<ClaudeEvent>,
) -> Result<(), String> {
    let _ = events.send(ClaudeEvent::Connecting).await;
    let mut builder = ClaudeCliBuilder::new()
        .command(command)
        .working_directory(cwd);
    if let Some(model) = model {
        builder = builder.model(model.cli_arg());
    }
    builder = builder.append_system_prompt(
        "You are ShellRS's AI assistant. Use the shellrs command for commands, files and \
         transfers on the current host; external CLI must be enabled in ShellRS settings. \
         Do not invent SSH commands or ask for passwords; the ShellRS app owns the connection \
         and credentials.",
    );
    let (permission_mode, ask_before_tools, bypass_permissions) =
        permission_policy(background, confirmation);
    builder = builder
        .permission_mode(permission_mode)
        .dangerously_skip_permissions(bypass_permissions);
    builder = if resume {
        builder.resume(Some(session_id.to_string()))
    } else {
        builder.session_id(session_id)
    };
    if background && !bypass_permissions {
        builder = builder.permission_prompt_tool("stdio");
    }
    let mut command = builder
        .build_command()
        .map_err(|error| t!("ai.claude.spawn_failed", error = error).to_string())?;
    command.arg("--include-partial-messages");
    command.env("PATH", claude_process_path());
    #[cfg(windows)]
    let command = windows_batch_command(command);
    let child = command
        .spawn()
        .map_err(|error| t!("ai.claude.spawn_failed", error = error).to_string())?;
    let mut client = AsyncClient::new(child)
        .map_err(|error| t!("ai.claude.connect_failed", error = error).to_string())?;
    if background && !bypass_permissions {
        client.enable_tool_approval().await.map_err(|error| {
            t!("ai.claude.permission_channel_failed", error = error).to_string()
        })?;
    }
    events
        .send(ClaudeEvent::Ready)
        .await
        .map_err(|_| t!("ai.claude.panel_closed").to_string())?;

    let mut commands_open = true;
    while let Ok(prompt) = prompts.recv().await {
        let mut blocks = Vec::with_capacity(1 + prompt.images.len());
        blocks.push(ContentBlock::Text(TextBlock {
            text: prompt.text,
            citations: Vec::new(),
        }));
        for (media_type, data) in prompt.images {
            blocks.push(ContentBlock::Image(ImageBlock {
                source: ImageSource {
                    source_type: ImageSourceType::Base64,
                    media_type,
                    data,
                },
            }));
        }
        client
            .send(&ClaudeInput::user_message_blocks(blocks, session_id))
            .await
            .map_err(|error| t!("ai.claude.write_failed", error = error).to_string())?;
        let mut streamed = false;
        let mut thinking_streamed = false;
        loop {
            let output = tokio::select! {
                command = commands.recv(), if commands_open => {
                    match command {
                        Ok(ClaudeCommand::Interrupt) => {
                            client
                                .interrupt()
                                .await
                                .map_err(|error| {
                                    t!("ai.claude.stop_failed", error = error).to_string()
                                })?;
                            continue;
                        }
                        Err(_) => {
                            commands_open = false;
                            continue;
                        }
                    }
                }
                output = client.receive() => {
                    output.map_err(|error| {
                        t!("ai.claude.connection_lost", error = error).to_string()
                    })?
                }
            };
            match output {
                ClaudeOutput::Assistant(message) => {
                    for block in message.message.content {
                        match block {
                            ContentBlock::Text(text) if !streamed => {
                                let _ = events.send(ClaudeEvent::Text(text.text)).await;
                            }
                            ContentBlock::Thinking(thinking)
                                if !thinking_streamed && !thinking.thinking.is_empty() =>
                            {
                                let _ = events.send(ClaudeEvent::Thinking(thinking.thinking)).await;
                            }
                            ContentBlock::ToolUse(tool) => {
                                let _ = events
                                    .send(ClaudeEvent::ToolCall {
                                        id: tool.id,
                                        name: tool.name,
                                        input: tool.input,
                                    })
                                    .await;
                            }
                            _ => {}
                        }
                    }
                }
                ClaudeOutput::User(message) => {
                    for block in message.message.content {
                        if let ContentBlock::ToolResult(result) = block {
                            let _ = events
                                .send(ClaudeEvent::ToolResult {
                                    id: result.tool_use_id,
                                    content: tool_result_text(result.content),
                                    is_error: result.is_error.unwrap_or(false),
                                })
                                .await;
                        }
                    }
                }
                ClaudeOutput::StreamEvent(message) => {
                    if let Some(thinking) = stream_thinking(&message.event) {
                        thinking_streamed = true;
                        let _ = events.send(ClaudeEvent::Thinking(thinking)).await;
                    } else if let Some(text) = stream_text(&message.event) {
                        streamed = true;
                        let _ = events.send(ClaudeEvent::Text(text)).await;
                    }
                }
                ClaudeOutput::ControlRequest(request) => {
                    if let ControlRequestPayload::CanUseTool(permission) = &request.request {
                        let _ = events
                            .send(ClaudeEvent::Permission {
                                request_id: request.request_id.clone(),
                                tool_name: permission.tool_name.clone(),
                                input: permission.input.clone(),
                                reason: permission
                                    .decision_reason
                                    .clone()
                                    .or_else(|| permission.blocked_path.clone()),
                            })
                            .await;
                        let response = if ask_before_tools {
                            let mut stopped = false;
                            let decision = loop {
                                tokio::select! {
                                    command = commands.recv(), if commands_open => {
                                        match command {
                                            Ok(ClaudeCommand::Interrupt) => {
                                                stopped = true;
                                                break None;
                                            }
                                            Err(_) => commands_open = false,
                                        }
                                    }
                                    decision = permissions.recv() => break Some(decision),
                                }
                            };
                            if stopped {
                                permission.deny_and_stop(
                                    "Stopped by the user in ShellRS",
                                    &request.request_id,
                                )
                            } else {
                                match decision {
                                    Some(Ok(PermissionDecision::Allow)) => {
                                        permission.allow(&request.request_id)
                                    }
                                    Some(Ok(PermissionDecision::Deny(reason))) => {
                                        permission.deny(reason, &request.request_id)
                                    }
                                    Some(Err(_)) | None => permission.deny(
                                        "ShellRS closed before answering",
                                        &request.request_id,
                                    ),
                                }
                            }
                        } else {
                            permission.allow(&request.request_id)
                        };
                        client
                            .send_control_response(response)
                            .await
                            .map_err(|error| {
                                t!("ai.claude.permission_reply_failed", error = error).to_string()
                            })?;
                    }
                }
                ClaudeOutput::ControlCancelRequest(request) => {
                    let _ = events
                        .send(ClaudeEvent::PermissionCancelled {
                            request_id: request.request_id,
                        })
                        .await;
                }
                ClaudeOutput::Result(result) => {
                    if let Some(usage) = result.usage.as_ref() {
                        let iteration = usage.iterations.last();
                        let input_tokens = iteration
                            .map(|iteration| u64::from(iteration.input_tokens))
                            .unwrap_or(u64::from(usage.input_tokens));
                        let output_tokens = iteration
                            .map(|iteration| u64::from(iteration.output_tokens))
                            .unwrap_or(u64::from(usage.output_tokens));
                        let cache_read_input_tokens = iteration
                            .and_then(|iteration| iteration.cache_read_input_tokens)
                            .map(u64::from)
                            .unwrap_or(u64::from(usage.cache_read_input_tokens));
                        let cache_creation_input_tokens = iteration
                            .and_then(|iteration| iteration.cache_creation_input_tokens)
                            .map(u64::from)
                            .unwrap_or(u64::from(usage.cache_creation_input_tokens));
                        let max_tokens = result
                            .model_usage
                            .as_ref()
                            .or(result.model_usage_since_last_result.as_ref())
                            .and_then(|models| {
                                models
                                    .values()
                                    .filter(|usage| usage.context_window > 0)
                                    .max_by_key(|usage| {
                                        usage.input_tokens
                                            + usage.output_tokens
                                            + usage.cache_read_input_tokens
                                            + usage.cache_creation_input_tokens
                                    })
                                    .map(|usage| usage.context_window)
                            })
                            .unwrap_or(0);
                        if max_tokens > 0 {
                            let _ = events
                                .send(ClaudeEvent::ContextUsage(ClaudeContextUsage {
                                    used_tokens: context_used_tokens(
                                        input_tokens,
                                        output_tokens,
                                        cache_read_input_tokens,
                                        cache_creation_input_tokens,
                                    ),
                                    max_tokens,
                                    input_tokens,
                                    output_tokens,
                                    cache_read_input_tokens,
                                    cache_creation_input_tokens,
                                }))
                                .await;
                        }
                    }
                    let _ = events.send(ClaudeEvent::Done).await;
                    break;
                }
                ClaudeOutput::Error(error) => {
                    let _ = events.send(ClaudeEvent::Error(format!("{error:?}"))).await;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// How a confirmation strategy reaches the Claude CLI.
///
/// Terminal mode only plans, so it never runs a tool. Automatic mode bypasses
/// permission prompts entirely. Balanced and ask-every-time use the host
/// approval channel; only the latter waits for the user.
fn permission_policy(
    background: bool,
    confirmation: AiConfirmationStrategy,
) -> (PermissionMode, bool, bool) {
    if !background {
        return (PermissionMode::Plan, false, false);
    }
    match confirmation {
        AiConfirmationStrategy::Balanced => (PermissionMode::AcceptEdits, false, false),
        AiConfirmationStrategy::AlwaysAsk => (PermissionMode::Default, true, false),
        AiConfirmationStrategy::AutoApprove => (PermissionMode::BypassPermissions, false, true),
    }
}

fn context_used_tokens(
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> u64 {
    input_tokens
        .saturating_add(output_tokens)
        .saturating_add(cache_read_input_tokens)
        .saturating_add(cache_creation_input_tokens)
}

/// Resolve a user-entered working directory, falling back to where ShellRS
/// was started and then the home directory.
pub fn working_directory(stored: Option<String>) -> PathBuf {
    stored
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| Path::new(".").to_path_buf())
}

fn stream_text(event: &serde_json::Value) -> Option<String> {
    if event.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    if delta.get("type")?.as_str()? != "text_delta" {
        return None;
    }
    delta
        .get("text")?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn stream_thinking(event: &serde_json::Value) -> Option<String> {
    if event.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let delta = event.get("delta")?;
    if delta.get("type")?.as_str()? != "thinking_delta" {
        return None;
    }
    delta
        .get("thinking")?
        .as_str()
        .filter(|thinking| !thinking.is_empty())
        .map(str::to_owned)
}

fn tool_result_text(content: Option<ToolResultContent>) -> String {
    match content {
        Some(ToolResultContent::Text(text)) => text,
        Some(ToolResultContent::Structured(blocks)) => {
            serde_json::to_string_pretty(&blocks).unwrap_or_else(|_| format!("{blocks:?}"))
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_strategies_select_the_expected_permission_policy() {
        assert_eq!(
            permission_policy(false, AiConfirmationStrategy::AlwaysAsk),
            (PermissionMode::Plan, false, false)
        );
        assert_eq!(
            permission_policy(true, AiConfirmationStrategy::AlwaysAsk),
            (PermissionMode::Default, true, false)
        );
        assert_eq!(
            permission_policy(true, AiConfirmationStrategy::Balanced),
            (PermissionMode::AcceptEdits, false, false)
        );
        assert_eq!(
            permission_policy(true, AiConfirmationStrategy::AutoApprove),
            (PermissionMode::BypassPermissions, false, true)
        );
    }

    #[test]
    fn context_usage_includes_cached_input_tokens() {
        assert_eq!(context_used_tokens(1_200, 300, 8_000, 500), 10_000);
    }

    #[test]
    #[cfg(unix)]
    fn command_lookup_searches_augmented_gui_paths() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let command = second.path().join("claude");
        std::fs::write(&command, b"fake").unwrap();

        let paths = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        assert_eq!(find_in_paths(&paths, "claude"), Some(command));
    }

    #[test]
    #[cfg(windows)]
    fn command_lookup_prefers_windows_executables_over_npm_shell_shims() {
        let folder = tempfile::tempdir().unwrap();
        for name in ["claude", "claude.cmd", "claude.exe"] {
            std::fs::write(folder.path().join(name), b"fake").unwrap();
        }
        let paths = vec![folder.path().to_path_buf()];
        assert_eq!(
            find_in_paths(&paths, "claude"),
            Some(folder.path().join("claude.exe"))
        );

        std::fs::remove_file(folder.path().join("claude.exe")).unwrap();
        assert_eq!(
            find_in_paths(&paths, "claude"),
            Some(folder.path().join("claude.cmd"))
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_batch_command_line_quotes_the_npm_shim_and_its_arguments() {
        let line = windows_batch_command_line(
            std::ffi::OsStr::new(r"C:\Program Files\nodejs\claude.cmd"),
            &[OsString::from("--print"), OsString::from("hello world")],
        );
        assert_eq!(
            line,
            OsString::from(
                "\"\"C:\\Program Files\\nodejs\\claude.cmd\" \"--print\" \"hello world\"\""
            )
        );
    }

    #[test]
    fn thinking_deltas_are_separate_from_answer_text() {
        let thinking = serde_json::json!({
            "type": "content_block_delta",
            "delta": { "type": "thinking_delta", "thinking": "先检查服务状态" }
        });
        assert_eq!(
            stream_thinking(&thinking).as_deref(),
            Some("先检查服务状态")
        );
        assert_eq!(stream_text(&thinking), None);

        let text = serde_json::json!({
            "type": "content_block_delta",
            "delta": { "type": "text_delta", "text": "回答" }
        });
        assert_eq!(stream_thinking(&text), None);
    }

    #[cfg(unix)]
    #[test]
    fn queued_prompts_are_both_consumed_after_the_first_turn_finishes() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::{Duration, Instant};

        let folder = tempfile::tempdir().unwrap();
        let command = folder.path().join("fake-claude");
        std::fs::write(
            &command,
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"duration_api_ms":1,"num_turns":1,"session_id":"00000000-0000-0000-0000-000000000000","total_cost_usd":0.0,"permission_denials":[]}'
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (prompt_tx, prompt_rx) = async_channel::unbounded();
        let (_command_tx, command_rx) = async_channel::unbounded();
        let (_permission_tx, permission_rx) = async_channel::unbounded();
        let (event_tx, event_rx) = async_channel::unbounded();
        let session = Uuid::new_v4();
        let worker = spawn(
            command,
            folder.path().to_path_buf(),
            None,
            false,
            AiConfirmationStrategy::Balanced,
            session,
            false,
            prompt_rx,
            command_rx,
            permission_rx,
            event_tx,
        );
        for text in ["first", "second"] {
            prompt_tx
                .send_blocking(ClaudePrompt {
                    text: text.into(),
                    images: Vec::new(),
                })
                .unwrap();
        }
        drop(prompt_tx);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut done = 0;
        while done < 2 && Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(ClaudeEvent::Done) => done += 1,
                Ok(ClaudeEvent::Error(error)) => panic!("fake Claude failed: {error}"),
                Ok(_) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => break,
            }
        }
        assert_eq!(done, 2, "both queued prompts should finish");
        worker.join().unwrap();
    }
}
