//! Bounded native command logs. Consumers provide task IDs, never paths.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, task};

#[derive(Clone)]
pub(super) struct Binding {
    pub(super) session: String,
    pub(super) task: String,
    pub(super) path: Option<PathBuf>,
    pub(super) terminal: bool,
    pub(super) agent: bool,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    session: String,
    task: String,
    file: String,
    offset: u64,
}
const PREFIX: &str = "keel:claude-output:v1:";

pub(super) fn read(binding: Binding, query: task::OutputQuery) -> Result<task::OutputPage> {
    let limit = query.limit.unwrap_or(16384) as usize;
    if !(4..=65536).contains(&limit) {
        return Err(invalid("output limit", &limit.to_string()));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|s| {
            let c: Cursor = serde_json::from_str(
                s.strip_prefix(PREFIX)
                    .ok_or_else(|| invalid("output cursor", s))?,
            )
            .map_err(|_| invalid("output cursor", s))?;
            if c.session != binding.session || c.task != binding.task {
                return Err(invalid("output cursor", s));
            }
            Ok(c)
        })
        .transpose()?;
    let unavailable = || task::OutputPage {
        task_id: binding.task.clone(),
        text: String::new(),
        next_cursor: None,
        availability: task::OutputAvailability::Unavailable,
        eof: true,
    };
    // Agent-run output files are complete JSONL transcripts, including native
    // system attachments. Use owned agent history instead of dumping them as logs.
    if binding.agent {
        return Ok(unavailable());
    }
    let Some(path) = &binding.path else {
        return Ok(task::OutputPage {
            availability: if binding.terminal {
                task::OutputAvailability::Unavailable
            } else {
                task::OutputAvailability::Unknown
            },
            ..unavailable()
        });
    };
    if !path.is_absolute()
        || path.file_name().and_then(|s| s.to_str()) != Some(&format!("{}.output", binding.task))
        || path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            != Some("tasks")
        || path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            != Some(&binding.session)
    {
        return Ok(unavailable());
    }
    let canonical = match path.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(unavailable()),
        Err(e) => return Err(e.into()),
    };
    let directory = path.parent().unwrap().canonicalize()?;
    if canonical.parent() != Some(directory.as_path()) {
        return Ok(unavailable());
    }
    let mut file = File::open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(unavailable());
    }
    let fingerprint = fingerprint(&metadata);
    let offset = cursor.as_ref().map_or(0, |c| c.offset);
    if cursor
        .as_ref()
        .is_some_and(|c| c.file != fingerprint || offset > metadata.len())
    {
        return Err(invalid("output cursor", query.cursor.as_deref().unwrap()));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit as u64).read_to_end(&mut bytes)?;
    // Keep a trailing incomplete UTF-8 character for the next read.
    if let Err(e) = std::str::from_utf8(&bytes)
        && e.error_len().is_none()
        && !(binding.terminal && offset + bytes.len() as u64 >= metadata.len())
    {
        bytes.truncate(e.valid_up_to());
    }
    let next = offset + bytes.len() as u64;
    let eof = next >= metadata.len();
    let next_cursor = if binding.terminal && eof {
        None
    } else {
        Some(format!(
            "{PREFIX}{}",
            serde_json::to_string(&Cursor {
                session: binding.session,
                task: binding.task.clone(),
                file: fingerprint,
                offset: next
            })
            .unwrap()
        ))
    };
    Ok(task::OutputPage {
        task_id: binding.task,
        text: String::from_utf8_lossy(&bytes).into_owned(),
        next_cursor,
        eof,
        availability: if binding.terminal {
            task::OutputAvailability::Complete
        } else {
            task::OutputAvailability::Partial
        },
    })
}

fn invalid(setting: &'static str, value: &str) -> Error {
    Error::InvalidSetting {
        setting,
        value: value.into(),
        reason:
            "Cursor must belong to this task and unchanged native file; limit must be 4..=65536"
                .into(),
    }
}

fn fingerprint(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", metadata.created().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paginated_logs_follow_appends_and_reject_cross_task_rotation_and_truncation() {
        let dir = std::env::temp_dir()
            .join(format!("keel-logs-{}", uuid::Uuid::new_v4()))
            .join("root/tasks");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("task.output");
        fs::write(&path, "abc€tail").unwrap();
        let binding = Binding {
            session: "root".into(),
            task: "task".into(),
            path: Some(path.clone()),
            terminal: false,
            agent: false,
        };
        let page = read(
            binding.clone(),
            task::OutputQuery {
                limit: Some(4),
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(page.text, "abc");
        let cursor = page.next_cursor.clone();
        let page = read(
            binding.clone(),
            task::OutputQuery {
                limit: Some(4),
                cursor: cursor.clone(),
            },
        )
        .unwrap();
        assert_eq!(page.text, "€t");
        let mut other = binding.clone();
        other.task = "other".into();
        assert!(
            read(
                other,
                task::OutputQuery {
                    cursor: cursor.clone(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        fs::write(&path, "a").unwrap();
        assert!(
            read(
                binding.clone(),
                task::OutputQuery {
                    cursor: cursor.clone(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        fs::rename(&path, dir.join("old.output")).unwrap();
        fs::write(&path, "new contents").unwrap();
        assert!(
            read(
                binding.clone(),
                task::OutputQuery {
                    cursor,
                    ..Default::default()
                }
            )
            .is_err()
        );
        let page = read(
            Binding {
                terminal: true,
                ..binding
            },
            Default::default(),
        )
        .unwrap();
        assert_eq!(page.text, "new contents");
        assert_eq!(page.availability, task::OutputAvailability::Complete);
        assert!(page.next_cursor.is_none());
        fs::write(&path, b"ok\xe2").unwrap();
        let live = read(
            Binding {
                session: "root".into(),
                task: "task".into(),
                path: Some(path.clone()),
                terminal: false,
                agent: false,
            },
            Default::default(),
        )
        .unwrap();
        assert_eq!(live.text, "ok");
        assert!(!live.eof);
        let final_page = read(
            Binding {
                session: "root".into(),
                task: "task".into(),
                path: Some(path),
                terminal: true,
                agent: false,
            },
            task::OutputQuery {
                cursor: live.next_cursor,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(final_page.text, "\u{fffd}");
        assert!(final_page.eof);
        assert!(final_page.next_cursor.is_none());
        fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
    }
}
