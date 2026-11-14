//! Claude Code's own session transcripts, for the AI sidebar's history
//! dialog. Claude writes one JSON-lines file per session under
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
pub struct ConversationMessage {
    pub role: Role,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub file: PathBuf,
    pub title: String,
    pub updated: SystemTime,
    pub turns: usize,
    pub model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub summary: ConversationSummary,
    pub messages: Vec<ConversationMessage>,
}

pub fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let encoded = cwd
        .to_string_lossy()
        .replace(['/', '\\', ':'], "-")
        .to_string();
    Some(home.join(".claude").join("projects").join(encoded))
}

pub fn sessions(cwd: &Path) -> Vec<ConversationSummary> {
    project_dir(cwd)
        .map(|dir| sessions_in(&dir))
        .unwrap_or_default()
}

pub fn sessions_in(project: &Path) -> Vec<ConversationSummary> {
    let Ok(entries) = fs::read_dir(project) else {
        return Vec::new();
    };
    let mut sessions = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file = entry.path();
            if file.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                return None;
            }
            summarize(&file)
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated));
    sessions
}

pub fn load(summary: &ConversationSummary) -> Option<Conversation> {
    let text = fs::read_to_string(&summary.file).ok()?;
    let mut messages = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = user_text(&value) {
                    messages.push(ConversationMessage {
                        role: Role::User,
                        text,
                    });
                }
            }
            Some("assistant") => {
                for text in assistant_text(&value) {
                    messages.push(ConversationMessage {
                        role: Role::Assistant,
                        text,
                    });
                }
            }
            _ => {}
        }
    }
    Some(Conversation {
        summary: summary.clone(),
        messages,
    })
}

/// Remove this project's Claude transcripts. The caller asks first.
pub fn clear(cwd: &Path) -> io::Result<usize> {
    let Some(project) = project_dir(cwd) else {
        return Ok(0);
    };
    let Ok(entries) = fs::read_dir(project) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            && fs::remove_file(path).is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}

fn summarize(file: &Path) -> Option<ConversationSummary> {
    let id = Uuid::parse_str(file.file_stem()?.to_str()?).ok()?;
    let text = fs::read_to_string(file).ok()?;
    let mut title = None;
    let mut turns = 0;
    let mut model = None;
    let mut last_timestamp = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(timestamp) = value.get("timestamp").and_then(Value::as_str) {
            last_timestamp = Some(timestamp.to_string());
        }
        match value.get("type").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = user_text(&value) {
                    turns += 1;
                    if title.is_none() {
                        title = Some(title_of(&text));
                    }
                }
            }
            Some("assistant") if model.is_none() => {
                model = value
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    let updated = last_timestamp
        .as_deref()
        .and_then(parse_time)
        .or_else(|| fs::metadata(file).ok()?.modified().ok())
        .unwrap_or(UNIX_EPOCH);
    Some(ConversationSummary {
        id,
        file: file.to_path_buf(),
        title: title.unwrap_or_else(|| "Untitled".into()),
        updated,
        turns,
        model,
    })
}

fn parse_time(timestamp: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(SystemTime::from)
}

fn user_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    let text = content_text(content)?;
    let text = text.trim();
    if text.is_empty() || text.starts_with("<system-reminder>") || text.starts_with("<file ") {
        return None;
    }
    let text = text
        .strip_prefix("SSH host: ")
        .and_then(|rest| rest.split_once("\n\n").map(|(_, prompt)| prompt))
        .unwrap_or(text)
        .trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn assistant_text(value: &Value) -> Vec<String> {
    let Some(Value::Array(blocks)) = value.pointer("/message/content") else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
        .collect()
}

fn title_of(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut title = line.chars().take(80).collect::<String>();
    if line.chars().count() > 80 {
        title.push('…');
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn claude_project_directories_keep_the_leading_dash() {
        let dir = project_dir(Path::new("/Users/me/project")).unwrap();
        assert_eq!(dir.file_name().unwrap(), "-Users-me-project");
    }

    #[test]
    fn a_transcript_gives_its_title_turns_and_model() {
        let root = tempfile::tempdir().unwrap();
        let file = root
            .path()
            .join("59453746-2cf6-4b2d-a5f7-05127103348b.jsonl");
        let mut transcript = fs::File::create(&file).unwrap();
        for line in [
            r#"{"type":"user","sessionId":"59453746-2cf6-4b2d-a5f7-05127103348b","timestamp":"2026-11-13T14:28:59.116Z","message":{"role":"user","content":[{"type":"text","text":"SSH host: web-01\n\nanalyze the disk"}]}}"#,
            r#"{"type":"assistant","sessionId":"59453746-2cf6-4b2d-a5f7-05127103348b","timestamp":"2026-11-13T14:29:19.273Z","message":{"model":"claude-sonnet-5-5","content":[{"type":"text","text":"Check df -h first."}]}}"#,
            r#"{"type":"user","sessionId":"59453746-2cf6-4b2d-a5f7-05127103348b","timestamp":"2026-11-13T14:30:00.000Z","message":{"role":"user","content":[{"type":"text","text":"continue"}]}}"#,
        ] {
            writeln!(transcript, "{line}").unwrap();
        }

        let sessions = sessions_in(root.path());
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.title, "analyze the disk");
        assert_eq!(session.turns, 2);
        assert_eq!(session.model.as_deref(), Some("claude-sonnet-5-5"));

        let conversation = load(session).unwrap();
        assert_eq!(conversation.messages.len(), 3);
        assert_eq!(conversation.messages[0].role, Role::User);
        assert_eq!(conversation.messages[0].text, "analyze the disk");
        assert_eq!(conversation.messages[1].role, Role::Assistant);
        assert_eq!(conversation.messages[2].text, "continue");
    }
}
