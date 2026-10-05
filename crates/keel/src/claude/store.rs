//! Reads Claude Code's stored sessions directly from disk.
//!
//! Each session is a JSONL transcript at
//! `<config dir>/projects/<encoded cwd>/<session id>.jsonl`, where the
//! directory name is the working directory with every character other than
//! an ASCII letter or digit replaced by `-`. The format is Claude Code's own
//! and undocumented, so it is parsed tolerantly: unknown entries are skipped.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::session;

/// Claude Code shortens longer directory names and adds a hash suffix.
const MAX_ENCODED_LEN: usize = 200;

pub(super) struct Store {
    projects: PathBuf,
}

pub(super) struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    /// Last modification, in seconds since the Unix epoch.
    pub modified: i64,
}

impl Store {
    pub(super) fn new(config_dir: &Path) -> Self {
        Self {
            projects: config_dir.join("projects"),
        }
    }

    /// Session files, most recently modified first. `cwd` limits the list to
    /// one working directory.
    pub(super) fn list(&self, cwd: Option<&Path>) -> io::Result<Vec<SessionFile>> {
        let dirs = match cwd {
            Some(cwd) => self.project_dirs(cwd)?,
            None => subdirectories(&self.projects)?,
        };
        let mut files = Vec::new();
        for dir in dirs {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                files.push(SessionFile {
                    id: id.to_owned(),
                    modified: modified_secs(&path),
                    path: path.clone(),
                });
            }
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.modified));
        Ok(files)
    }

    pub(super) fn find(&self, id: &str) -> io::Result<Option<SessionFile>> {
        let name = format!("{id}.jsonl");
        for dir in subdirectories(&self.projects)? {
            let path = dir.join(&name);
            if path.is_file() {
                return Ok(Some(SessionFile {
                    id: id.to_owned(),
                    modified: modified_secs(&path),
                    path,
                }));
            }
        }
        Ok(None)
    }

    fn project_dirs(&self, cwd: &Path) -> io::Result<Vec<PathBuf>> {
        let encoded = encode(cwd);
        if encoded.len() <= MAX_ENCODED_LEN {
            let dir = self.projects.join(&encoded);
            return Ok(if dir.is_dir() { vec![dir] } else { Vec::new() });
        }
        let prefix = &encoded[..MAX_ENCODED_LEN];
        Ok(subdirectories(&self.projects)?
            .into_iter()
            .filter(|dir| {
                dir.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .collect())
    }
}

/// Every JSON line of a transcript; unreadable lines are skipped.
pub(super) fn read_entries(path: &Path) -> io::Result<Vec<Value>> {
    let reader = BufReader::new(fs::File::open(path)?);
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

pub(super) fn summary(file: &SessionFile, entries: &[Value]) -> session::Summary {
    let mut preview = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut cwd = None;
    let mut model = None;
    let mut created = None;
    for entry in entries {
        let text = |key: &str| entry.get(key).and_then(Value::as_str);
        if created.is_none() {
            created = text("timestamp").and_then(timestamp_millis);
        }
        if cwd.is_none() {
            cwd = text("cwd").map(PathBuf::from);
        }
        match text("type") {
            Some("custom-title") => custom_title = text("customTitle").map(str::to_owned),
            Some("ai-title") => ai_title = text("aiTitle").map(str::to_owned),
            Some("user") if preview.is_none() && is_main(entry) => {
                preview = entry
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(first_text);
            }
            Some("assistant") if is_main(entry) => {
                let name = entry
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str);
                if let Some(name) = name.filter(|name| !name.starts_with('<')) {
                    model = Some(name.to_owned());
                }
            }
            _ => {}
        }
    }
    session::Summary {
        id: file.id.clone(),
        provider: super::PROVIDER_ID.to_owned(),
        title: custom_title.or(ai_title),
        preview: preview.unwrap_or_default(),
        cwd: cwd.unwrap_or_default(),
        model,
        effort: None,
        status: session::Status::NotLoaded,
        created_at: created.map_or(file.modified, |ms| ms / 1000),
        updated_at: file.modified,
        source: None,
    }
}

/// A user or assistant entry of the main conversation, not a sub-agent's
/// side chain or an injected meta message.
pub(super) fn is_main(entry: &Value) -> bool {
    let flag = |key: &str| entry.get(key).and_then(Value::as_bool) == Some(true);
    !flag("isSidechain") && !flag("isMeta")
}

fn first_text(content: &Value) -> Option<String> {
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

/// Milliseconds since the Unix epoch for an RFC 3339 UTC timestamp such as
/// `2026-10-01T18:00:43.687Z`.
pub(super) fn timestamp_millis(text: &str) -> Option<i64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;

    // Days since the epoch for a proleptic Gregorian date (Howard Hinnant).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

fn encode(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn subdirectories(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(dir) {
        Ok(entries) => Ok(entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn modified_secs(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn encodes_working_directories_like_claude_code() {
        assert_eq!(
            encode(Path::new("/mnt/data/spelvia/keel")),
            "-mnt-data-spelvia-keel"
        );
        assert_eq!(encode(Path::new("/tmp/keel_test.ws")), "-tmp-keel-test-ws");
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(timestamp_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            timestamp_millis("2026-10-01T18:00:43.687Z"),
            Some(1_790_877_643_687)
        );
        assert_eq!(timestamp_millis("not a time"), None);
    }

    #[test]
    fn summarizes_a_transcript() {
        let file = SessionFile {
            id: "s1".into(),
            path: PathBuf::new(),
            modified: 99,
        };
        let entries = vec![
            json!({ "type": "queue-operation", "timestamp": "1970-01-01T00:00:10.000Z" }),
            json!({ "type": "user", "cwd": "/repo", "message": { "content": "fix the bug" } }),
            json!({ "type": "assistant", "message": { "model": "claude-sonnet-4-6", "content": [] } }),
            json!({ "type": "ai-title", "aiTitle": "Bug fix" }),
        ];
        let summary = summary(&file, &entries);
        assert_eq!(summary.title.as_deref(), Some("Bug fix"));
        assert_eq!(summary.preview, "fix the bug");
        assert_eq!(summary.cwd, PathBuf::from("/repo"));
        assert_eq!(summary.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!((summary.created_at, summary.updated_at), (10, 99));
    }
}
