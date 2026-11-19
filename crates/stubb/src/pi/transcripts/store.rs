//! Reads Pi's stored sessions directly from disk.
//!
//! Each session is a JSONL file at
//! `<agent dir>/sessions/--<encoded cwd>--/<timestamp>_<session id>.jsonl`,
//! where the encoded directory drops the leading separator and replaces
//! `/`, `\` and `:` with `-`. The first line is a `session` header; the
//! remaining entries form a tree through `id` and `parentId`. The format is
//! documented in Pi's `docs/session-format.md`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::runtime::files::{modified_secs, subdirectories, timestamp_millis};
use crate::session;

pub(in crate::pi) struct Store {
    sessions: PathBuf,
}

pub(in crate::pi) struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    /// Last modification, in seconds since the Unix epoch.
    pub modified: i64,
}

impl Store {
    pub(in crate::pi) fn new(agent_dir: &Path) -> Self {
        Self {
            sessions: agent_dir.join("sessions"),
        }
    }

    /// Session files, most recently modified first. `cwd` limits the list to
    /// one working directory.
    pub(in crate::pi) fn list(&self, cwd: Option<&Path>) -> io::Result<Vec<SessionFile>> {
        let dirs = match cwd {
            Some(cwd) => vec![self.sessions.join(encode(cwd))],
            None => subdirectories(&self.sessions)?,
        };
        let mut files = Vec::new();
        for dir in dirs {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            files.extend(
                entries
                    .flatten()
                    .filter_map(|entry| session_file(entry.path())),
            );
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.modified));
        Ok(files)
    }

    pub(in crate::pi) fn find(&self, id: &str) -> io::Result<Option<SessionFile>> {
        for dir in subdirectories(&self.sessions)? {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let found = entries
                .flatten()
                .filter_map(|entry| session_file(entry.path()))
                .find(|file| file.id == id);
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }
}

/// A `<timestamp>_<id>.jsonl` file.
fn session_file(path: PathBuf) -> Option<SessionFile> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let id = stem.split_once('_').map_or(stem, |(_, id)| id).to_owned();
    Some(SessionFile {
        id,
        modified: modified_secs(&path),
        path,
    })
}

pub(in crate::pi) fn summary(file: &SessionFile, entries: &[Value]) -> session::Summary {
    let mut cwd = None;
    let mut created = None;
    let mut title = None;
    let mut preview = None;
    let mut model = None;
    let mut effort = None;
    for entry in entries {
        let text = |key: &str| entry.get(key).and_then(Value::as_str);
        match text("type") {
            Some("session") => {
                cwd = text("cwd").map(PathBuf::from);
                created = text("timestamp").and_then(timestamp_millis);
            }
            Some("session_info") => title = text("name").map(str::to_owned),
            Some("model_change") => {
                if let (Some(provider), Some(id)) = (text("provider"), text("modelId")) {
                    model = Some(model_id(provider, id));
                }
            }
            Some("thinking_level_change") => effort = text("thinkingLevel").map(str::to_owned),
            Some("message") if preview.is_none() => {
                let message = entry.get("message").unwrap_or(&Value::Null);
                if message.get("role").and_then(Value::as_str) == Some("user") {
                    preview = message.get("content").and_then(first_text);
                }
            }
            _ => {}
        }
    }
    session::Summary {
        id: file.id.clone(),
        provider: crate::pi::PROVIDER_ID.to_owned(),
        title,
        preview: preview.unwrap_or_default(),
        cwd: cwd.unwrap_or_default(),
        model,
        effort,
        status: session::Status::NotLoaded,
        created_at: created.map_or(file.modified, |ms| ms / 1000),
        updated_at: file.modified,
        source: None,
    }
}

/// Stubb's model id for a Pi model: `<provider>/<model id>`, the form Pi's
/// `--model` option accepts.
pub(in crate::pi) fn model_id(provider: &str, id: &str) -> String {
    format!("{provider}/{id}")
}

/// Text of a user message's content, which is a string or a list of blocks.
pub(in crate::pi) fn first_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|block| block.get("text").and_then(Value::as_str))
            .map(str::to_owned),
        _ => None,
    }
}

fn encode(cwd: &Path) -> String {
    let path = cwd.to_string_lossy();
    let path = path.strip_prefix(['/', '\\']).unwrap_or(&path);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn encodes_working_directories_like_pi() {
        assert_eq!(
            encode(Path::new("/work/projects/stubb")),
            "--work-projects-stubb--"
        );
        assert_eq!(
            encode(Path::new("/tmp/stubb_test.ws")),
            "--tmp-stubb_test.ws--"
        );
    }

    #[test]
    fn reads_the_id_from_the_file_name() {
        let file = session_file(PathBuf::from(
            "/s/--tmp--/2026-10-05T10-12-40-756Z_01a10b8d-20b3-76d3-89e6-446eaa136674.jsonl",
        ))
        .unwrap();
        assert_eq!(file.id, "01a10b8d-20b3-76d3-89e6-446eaa136674");
        assert!(session_file(PathBuf::from("/s/--tmp--/notes.txt")).is_none());
    }

    #[test]
    fn summarizes_a_session() {
        let file = SessionFile {
            id: "s1".into(),
            path: PathBuf::new(),
            modified: 99,
        };
        let entries = vec![
            json!({ "type": "session", "id": "s1", "timestamp": "1970-01-01T00:00:10.000Z", "cwd": "/repo" }),
            json!({ "type": "model_change", "provider": "vllm", "modelId": "qwen" }),
            json!({ "type": "thinking_level_change", "thinkingLevel": "medium" }),
            json!({ "type": "message", "message": { "role": "system", "content": "" } }),
            json!({ "type": "message", "message": { "role": "user", "content": [{ "type": "text", "text": "fix the bug" }] } }),
            json!({ "type": "session_info", "name": "Bug fix" }),
        ];
        let summary = summary(&file, &entries);
        assert_eq!(summary.title.as_deref(), Some("Bug fix"));
        assert_eq!(summary.preview, "fix the bug");
        assert_eq!(summary.cwd, PathBuf::from("/repo"));
        assert_eq!(summary.model.as_deref(), Some("vllm/qwen"));
        assert_eq!(summary.effort.as_deref(), Some("medium"));
        assert_eq!((summary.created_at, summary.updated_at), (10, 99));
    }
}
