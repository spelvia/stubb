//! A live session's title, followed in its transcript. Claude Code writes a
//! title as a `custom-title` entry (`/rename`) or an `ai-title` entry, and
//! has no stream message for either.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// Follows one transcript: each read takes only the lines appended since the
/// last, so a long session costs no more per turn than a short one.
#[derive(Debug, Default)]
pub(in crate::providers::claude) struct TitleReader {
    offset: u64,
    custom: Option<String>,
    ai: Option<String>,
}

impl TitleReader {
    /// Reads what was appended; the title as it now stands, if there is one.
    /// A title someone set wins over the one Claude generated.
    pub(in crate::providers::claude) fn read(&mut self, path: &Path) -> io::Result<Option<String>> {
        let mut file = File::open(path)?;
        if file.metadata()?.len() < self.offset {
            // Rewritten from the start: read it again.
            *self = Self::default();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)?;
        // A line still being written is read next time.
        let complete = appended
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1);
        self.offset += complete as u64;
        for line in appended[..complete].split(|byte| *byte == b'\n') {
            self.line(line);
        }
        Ok(self.custom.clone().or_else(|| self.ai.clone()))
    }

    fn line(&mut self, line: &[u8]) {
        let custom = contains(line, b"\"custom-title\"");
        if !custom && !contains(line, b"\"ai-title\"") {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned)
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("custom-title") => self.custom = text("customTitle"),
            Some("ai-title") => self.ai = text("aiTitle"),
            _ => {}
        }
    }
}

fn contains(line: &[u8], needle: &[u8]) -> bool {
    line.windows(needle.len()).any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn follows_titles_appended_to_a_transcript() {
        let dir = std::env::temp_dir().join(format!("stubb-title-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut file = File::create(&path).unwrap();
        let mut reader = TitleReader::default();
        writeln!(
            file,
            r#"{{"type":"user","message":{{"content":"fix the bug"}}}}"#
        )
        .unwrap();
        assert_eq!(reader.read(&path).unwrap(), None);

        writeln!(file, r#"{{"type":"ai-title","aiTitle":"Fix login bug"}}"#).unwrap();
        assert_eq!(
            reader.read(&path).unwrap().as_deref(),
            Some("Fix login bug")
        );

        // Half a line waits for the rest.
        write!(file, r#"{{"type":"custom-title","#).unwrap();
        assert_eq!(
            reader.read(&path).unwrap().as_deref(),
            Some("Fix login bug")
        );
        writeln!(file, r#""customTitle":"Login"}}"#).unwrap();
        assert_eq!(reader.read(&path).unwrap().as_deref(), Some("Login"));

        // A newer generated title does not replace the one someone set.
        writeln!(file, r#"{{"type":"ai-title","aiTitle":"Other"}}"#).unwrap();
        assert_eq!(reader.read(&path).unwrap().as_deref(), Some("Login"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
