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

/// The offset encoded in a page cursor; Stubb's file-backed cursors are offsets.
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

/// Every line of a file that holds a JSON value, as written and parsed.
/// Lines that are not JSON, such as a partly written last line, are left
/// out.
pub(crate) fn read_lines(path: &Path) -> io::Result<Vec<(String, Value)>> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut lines = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if let Ok(value) = serde_json::from_str(&line) {
            lines.push((line, value));
        }
    }
    Ok(lines)
}

/// Writes a new JSONL file of `lines` at `path`, all at once: the lines go
/// to a temporary file next to it, which is then renamed. Fails if `path`
/// exists.
pub(crate) fn write_new_lines<S: AsRef<str>>(path: &Path, lines: &[S]) -> io::Result<()> {
    use std::io::Write;

    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", path.display()),
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = io::BufWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?,
        );
        for line in lines {
            file.write_all(line.as_ref().as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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

/// Milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// The RFC 3339 UTC timestamp with milliseconds for `millis` since the Unix
/// epoch, such as `2026-10-01T18:00:43.687Z`; the inverse of
/// [`timestamp_millis`].
pub(crate) fn rfc3339_millis(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let of_day = millis.rem_euclid(86_400_000);
    // Civil date from days since the epoch (Howard Hinnant).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3_600_000,
        of_day / 60_000 % 60,
        of_day / 1000 % 60,
        of_day % 1000
    )
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

    #[test]
    fn formats_timestamps() {
        for text in [
            "1970-01-01T00:00:00.000Z",
            "2026-10-01T18:00:43.687Z",
            "2000-02-29T23:59:59.999Z",
            "2026-12-31T00:00:00.001Z",
        ] {
            assert_eq!(rfc3339_millis(timestamp_millis(text).unwrap()), text);
        }
    }

    #[test]
    fn writes_new_files_whole() {
        let dir = std::env::temp_dir().join(format!("stubb-files-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("s.jsonl");
        write_new_lines(&path, &["{\"a\":1}", "{\"b\":2}"]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(read_lines(&path).unwrap().len(), 2);
        let again = write_new_lines(&path, &["{}"]).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        // No temporary file is left behind.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
