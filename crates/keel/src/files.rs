//! Helpers for providers that read stored sessions from JSONL files.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::{Error, Result};

/// Runs file-system work off the async runtime.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Io(io::Error::other(error)))?
}

/// The offset encoded in a page cursor; Keel's file-backed cursors are offsets.
pub(crate) fn cursor_offset(cursor: Option<&str>) -> usize {
    cursor.and_then(|cursor| cursor.parse().ok()).unwrap_or(0)
}

/// Seconds since the Unix epoch.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// Every JSON line of a file; unreadable lines are skipped.
pub(crate) fn read_entries(path: &Path) -> io::Result<Vec<Value>> {
    let reader = BufReader::new(fs::File::open(path)?);
    Ok(reader
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

/// Directories directly inside `dir`; none if `dir` does not exist.
pub(crate) fn subdirectories(dir: &Path) -> io::Result<Vec<PathBuf>> {
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

/// Last modification, in seconds since the Unix epoch.
pub(crate) fn modified_secs(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Milliseconds since the Unix epoch for an RFC 3339 UTC timestamp such as
/// `2026-10-01T18:00:43.687Z`.
pub(crate) fn timestamp_millis(text: &str) -> Option<i64> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(timestamp_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            timestamp_millis("2026-10-01T18:00:43.687Z"),
            Some(1_790_877_643_687)
        );
        assert_eq!(timestamp_millis("not a time"), None);
    }
}
