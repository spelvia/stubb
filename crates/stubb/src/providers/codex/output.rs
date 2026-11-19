//! Bounded command logs, kept from native command events or read from stored
//! rollouts. Consumers provide task IDs and opaque cursors, never paths.

use serde::{Deserialize, Serialize};

use crate::{Error, Result, observation::task};

/// Bytes of output retained per live command. Older output is dropped from
/// the head, and reads report it as partial.
pub(super) const RETAINED: usize = 1 << 20;
const PREFIX: &str = "keel:codex-output:v1:";
const DEFAULT_LIMIT: u32 = 16384;

/// A command's output as received: absolute byte offsets stay stable while
/// the head is dropped, so cursors keep following a growing log.
pub(super) struct Buffer {
    generation: String,
    text: String,
    dropped: u64,
    /// Output produced before Stubb saw the command start may be missing.
    pub(super) head_missing: bool,
}

impl Buffer {
    pub(super) fn new(head_missing: bool) -> Self {
        Self {
            generation: uuid::Uuid::new_v4().to_string(),
            text: String::new(),
            dropped: 0,
            head_missing,
        }
    }

    pub(super) fn push(&mut self, text: &str) {
        self.text.push_str(text);
        if self.text.len() > RETAINED {
            let mut cut = self.text.len() - RETAINED;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn is_empty(&self) -> bool {
        self.dropped == 0 && self.text.is_empty()
    }

    /// Whether earlier output is known or possibly missing.
    pub(super) fn truncated(&self) -> bool {
        self.dropped > 0 || self.head_missing
    }

    pub(super) fn view<'a>(
        &'a self,
        session: &'a str,
        task: &'a str,
        terminal: bool,
        availability: task::OutputAvailability,
    ) -> View<'a> {
        View {
            session,
            task,
            generation: &self.generation,
            text: &self.text,
            start: self.dropped,
            terminal,
            availability,
        }
    }
}

/// Retained text of one task, starting at absolute byte offset `start`.
pub(super) struct View<'a> {
    pub(super) session: &'a str,
    pub(super) task: &'a str,
    /// Changes whenever offsets would no longer refer to the same bytes.
    pub(super) generation: &'a str,
    pub(super) text: &'a str,
    pub(super) start: u64,
    /// The command ended: its output will not grow.
    pub(super) terminal: bool,
    pub(super) availability: task::OutputAvailability,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    session: String,
    task: String,
    generation: String,
    offset: u64,
}

/// Reads up to `query.limit` bytes from the cursor, or from the oldest
/// retained byte. A cursor behind dropped output skips to the oldest
/// retained byte; the page then reports partial availability.
pub(super) fn page(view: View<'_>, query: task::OutputQuery) -> Result<task::OutputPage> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT) as usize;
    if !(4..=65536).contains(&limit) {
        return Err(invalid("output limit", &limit.to_string()));
    }
    let end = view.start + view.text.len() as u64;
    let mut availability = view.availability;
    let offset = match query.cursor.as_deref() {
        None => view.start,
        Some(value) => {
            let cursor: Cursor = value
                .strip_prefix(PREFIX)
                .and_then(|json| serde_json::from_str(json).ok())
                .ok_or_else(|| invalid("output cursor", value))?;
            if cursor.session != view.session
                || cursor.task != view.task
                || cursor.generation != view.generation
                || cursor.offset > end
            {
                return Err(invalid("output cursor", value));
            }
            if cursor.offset < view.start {
                availability = task::OutputAvailability::Partial;
                view.start
            } else {
                cursor.offset
            }
        }
    };
    let from = (offset - view.start) as usize;
    if !view.text.is_char_boundary(from) {
        return Err(invalid(
            "output cursor",
            query.cursor.as_deref().unwrap_or(""),
        ));
    }
    let mut to = (from + limit).min(view.text.len());
    // Keep a split UTF-8 character for the next read.
    while !view.text.is_char_boundary(to) {
        to -= 1;
    }
    let next = view.start + to as u64;
    let eof = next >= end;
    let next_cursor = (!(view.terminal && eof)).then(|| {
        format!(
            "{PREFIX}{}",
            serde_json::to_string(&Cursor {
                session: view.session.into(),
                task: view.task.into(),
                generation: view.generation.into(),
                offset: next,
            })
            .expect("cursor serialization")
        )
    });
    Ok(task::OutputPage {
        task_id: view.task.into(),
        text: view.text[from..to].into(),
        next_cursor,
        availability,
        eof,
    })
}

fn invalid(setting: &'static str, value: &str) -> Error {
    Error::InvalidSetting {
        setting,
        value: value.into(),
        reason: "Cursor must belong to this task and its retained output; limit must be 4..=65536"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(limit: u32, cursor: Option<String>) -> task::OutputQuery {
        task::OutputQuery {
            limit: Some(limit),
            cursor,
        }
    }

    #[test]
    fn pages_follow_appends_split_characters_and_end_at_terminal_eof() {
        let mut buffer = Buffer::new(false);
        buffer.push("abc€tail");
        let partial = task::OutputAvailability::Partial;
        let first = page(buffer.view("root", "task", false, partial), query(4, None)).unwrap();
        // A limit that ends inside a character stops before it.
        assert_eq!(first.text, "abc");
        assert!(!first.eof);
        let second = page(
            buffer.view("root", "task", false, partial),
            query(4, first.next_cursor.clone()),
        )
        .unwrap();
        assert_eq!(second.text, "€t");
        let rest = page(
            buffer.view("root", "task", false, partial),
            query(64, second.next_cursor),
        )
        .unwrap();
        assert_eq!(rest.text, "ail");
        assert!(rest.eof);
        // A running command's cursor waits at the end for more output.
        buffer.push(" more");
        let grown = page(
            buffer.view("root", "task", true, task::OutputAvailability::Complete),
            query(64, rest.next_cursor),
        )
        .unwrap();
        assert_eq!(grown.text, " more");
        assert!(grown.eof);
        assert!(grown.next_cursor.is_none());
        assert_eq!(grown.availability, task::OutputAvailability::Complete);
    }

    #[test]
    fn cursors_are_scoped_and_limits_bounded() {
        let mut buffer = Buffer::new(false);
        buffer.push("output");
        let unknown = task::OutputAvailability::Unknown;
        let cursor = page(buffer.view("root", "task", false, unknown), query(4, None))
            .unwrap()
            .next_cursor;
        for view in [
            buffer.view("root", "other", false, unknown),
            buffer.view("other", "task", false, unknown),
            Buffer::new(false).view("root", "task", false, unknown),
        ] {
            assert!(page(view, query(4, cursor.clone())).is_err());
        }
        for limit in [0, 3, 65537] {
            assert!(
                page(
                    buffer.view("root", "task", false, unknown),
                    query(limit, None)
                )
                .is_err()
            );
        }
        assert!(
            page(
                buffer.view("root", "task", false, unknown),
                query(4, Some("keel:claude-output:v1:{}".into()))
            )
            .is_err()
        );
    }

    #[test]
    fn dropped_head_keeps_offsets_and_reports_partial_output() {
        let mut buffer = Buffer::new(false);
        let complete = task::OutputAvailability::Complete;
        let early = page(buffer.view("root", "task", false, complete), query(4, None))
            .unwrap()
            .next_cursor;
        buffer.push("é");
        buffer.push(&"x".repeat(RETAINED));
        assert!(buffer.truncated());
        assert_eq!(buffer.text().len(), RETAINED);
        let skipped = page(buffer.view("root", "task", true, complete), query(4, early)).unwrap();
        assert_eq!(skipped.text, "xxxx");
        assert_eq!(skipped.availability, task::OutputAvailability::Partial);
    }
}
