//! Forks a stored Pi session the way Pi's own `/fork` and `/clone` do.
//!
//! Pi branches a session into a new file with `createBranchedSession`: the
//! new file holds a new `session` header that names the original in
//! `parentSession`, then the entries on the path from the root to the chosen
//! leaf. Labels are left out of that path, the remaining entries are linked
//! again in order, and the labels of kept entries are written again at the
//! end. Stubb does the same, with the leaf being the last entry before the
//! user message that starts the turn after [`session::Fork::at_turn`]
//! (the active branch's last entry for the whole session).
//!
//! [`session::Fork::at_turn`]: crate::session::Fork::at_turn

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::history;
use crate::providers::pi::events;

/// The entries of a forked session, its header first.
pub(in crate::providers::pi) struct Branch {
    pub entries: Vec<Value>,
}

/// Why a session cannot be forked.
#[derive(Debug, PartialEq)]
pub(in crate::providers::pi) enum Refusal {
    /// The active branch has no turn with this id.
    NoTurn,
    /// The file has no header, or is in Pi's first format, whose entries
    /// are not linked; Pi converts it when it next loads the session.
    Format,
}

/// The session `entries` of the file at `path`, branched into new session
/// `id` at `timestamp` (an RFC 3339 time).
pub(in crate::providers::pi) fn branch(
    entries: &[Value],
    path: &str,
    id: &str,
    timestamp: &str,
    at_turn: Option<&str>,
) -> Result<Branch, Refusal> {
    let header = entries
        .iter()
        .find(|entry| entry["type"] == "session")
        .ok_or(Refusal::Format)?;
    let version = header["version"].as_i64().unwrap_or(1);
    if version < 2 {
        return Err(Refusal::Format);
    }
    let mut path_entries = history::active_branch(entries);
    if let Some(at_turn) = at_turn {
        let starts = turn_starts(&path_entries);
        let position = starts
            .iter()
            .position(|(_, turn)| turn == at_turn)
            .ok_or(Refusal::NoTurn)?;
        if let Some((next, _)) = starts.get(position + 1) {
            path_entries.truncate(*next);
        }
    }

    // As createBranchedSession: labels leave the path, the rest is linked
    // again, and compactions follow a kept entry that replaced a label.
    let mut kept = Vec::new();
    let mut replacement: HashMap<&str, &str> = HashMap::new();
    let mut pending_labels = Vec::new();
    let mut parent = Value::Null;
    for entry in &path_entries {
        let entry_id = entry["id"].as_str().unwrap_or_default();
        if entry["type"] == "label" {
            pending_labels.push(entry_id);
            continue;
        }
        for label in pending_labels.drain(..) {
            replacement.insert(label, entry_id);
        }
        let mut entry = (*entry).clone();
        entry["parentId"] = parent;
        if entry["type"] == "compaction"
            && let Some(first) = entry["firstKeptEntryId"].as_str()
            && first != entry_id
            && let Some(other) = replacement.get(first)
        {
            entry["firstKeptEntryId"] = Value::String((*other).to_owned());
        }
        parent = Value::String(entry_id.to_owned());
        kept.push(entry);
    }

    // The labels of kept entries, as the whole file last set them.
    let mut ids: HashSet<String> = kept
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect();
    let mut labels: Vec<(String, Value, Value)> = Vec::new();
    for entry in entries.iter().filter(|entry| entry["type"] == "label") {
        let Some(target) = entry["targetId"].as_str() else {
            continue;
        };
        labels.retain(|(existing, _, _)| existing != target);
        if entry.get("label").is_some_and(|label| !label.is_null()) {
            labels.push((
                target.to_owned(),
                entry["label"].clone(),
                entry["timestamp"].clone(),
            ));
        }
    }
    let mut written_labels = Vec::new();
    for (target, label, at) in labels {
        if !ids.contains(&target) {
            continue;
        }
        let label_id = new_entry_id(&ids);
        ids.insert(label_id.clone());
        written_labels.push(json!({
            "type": "label",
            "id": label_id,
            "parentId": parent,
            "timestamp": at,
            "targetId": target,
            "label": label,
        }));
        parent = Value::String(label_id);
    }

    let mut new_header = json!({
        "type": "session",
        "version": version,
        "id": id,
        "timestamp": timestamp,
        "cwd": header["cwd"],
        "parentSession": path,
    });
    if header["cwd"].is_null() {
        new_header.as_object_mut().expect("an object").remove("cwd");
    }
    let mut branch = vec![new_header];
    branch.extend(kept);
    branch.extend(written_labels);
    Ok(Branch { entries: branch })
}

/// Where each turn of the branch starts, as [`history::turns`] groups them:
/// at each user message, and at the first message.
fn turn_starts(branch: &[&Value]) -> Vec<(usize, String)> {
    let mut starts = Vec::new();
    for (index, entry) in branch.iter().enumerate() {
        if entry["type"] != "message" {
            continue;
        }
        let message = &entry["message"];
        let role = message["role"].as_str();
        if !matches!(role, Some("user" | "assistant" | "toolResult")) {
            continue;
        }
        if role == Some("user") || starts.is_empty() {
            starts.push((index, events::turn_id(message)));
        }
    }
    starts
}

/// An 8-character hex id no kept entry has, like Pi's own entry ids.
fn new_entry_id(taken: &HashSet<String>) -> String {
    loop {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        if !taken.contains(&id) {
            return id;
        }
    }
}

/// The file name Pi gives a session: `<timestamp>_<id>.jsonl`, with `:` and
/// `.` in the timestamp replaced by `-`.
pub(in crate::providers::pi) fn file_name(timestamp: &str, id: &str) -> String {
    format!("{}_{id}.jsonl", timestamp.replace([':', '.'], "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, parent: Option<&str>, role: &str, at: i64) -> Value {
        json!({ "type": "message", "id": id, "parentId": parent, "timestamp": "2026-11-19T10:00:00.000Z",
            "message": { "role": role, "content": [{ "type": "text", "text": id }], "timestamp": at } })
    }

    /// Two turns on the active branch, an abandoned branch, labels and a
    /// model change between the turns.
    fn session() -> Vec<Value> {
        vec![
            json!({ "type": "session", "version": 3, "id": "old", "timestamp": "2026-11-19T10:00:00.000Z", "cwd": "/repo" }),
            json!({ "type": "model_change", "id": "m1", "parentId": null, "provider": "p", "modelId": "x" }),
            message("s1", Some("m1"), "system", 100),
            message("u1", Some("s1"), "user", 100),
            message("a1", Some("u1"), "assistant", 101),
            json!({ "type": "label", "id": "l1", "parentId": "a1", "targetId": "u1", "label": "start" }),
            message("t1", Some("l1"), "toolResult", 102),
            message("a2", Some("t1"), "assistant", 103),
            // Abandoned: a retry of the second prompt.
            message("x1", Some("a2"), "user", 104),
            json!({ "type": "thinking_level_change", "id": "k1", "parentId": "a2", "thinkingLevel": "high" }),
            message("u2", Some("k1"), "user", 105),
            message("a3", Some("u2"), "assistant", 106),
            json!({ "type": "label", "id": "l2", "parentId": "a3", "targetId": "u2", "label": "second" }),
        ]
    }

    fn ids(branch: &Branch) -> Vec<&str> {
        branch.entries[1..]
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn branches_through_a_turn_like_pi() {
        let branch = branch(
            &session(),
            "/s/old.jsonl",
            "new",
            "2026-11-19T11:00:00.000Z",
            Some("100"),
        )
        .unwrap();
        let header = &branch.entries[0];
        assert_eq!(header["id"], "new");
        assert_eq!(header["version"], 3);
        assert_eq!(header["cwd"], "/repo");
        assert_eq!(header["parentSession"], "/s/old.jsonl");
        // The label leaves the path, and the kept entries link up again;
        // the thinking level change before the next prompt stays.
        let kept = ids(&branch);
        assert_eq!(&kept[..6], ["m1", "s1", "u1", "a1", "t1", "a2"]);
        assert_eq!(kept.len(), 8);
        assert_eq!(kept[6], "k1");
        assert_eq!(branch.entries[1]["parentId"], Value::Null);
        for pair in branch.entries[1..].windows(2) {
            assert_eq!(pair[1]["parentId"], pair[0]["id"]);
        }
        // Only the label of a kept entry is written again, at the end.
        let label = branch.entries.last().unwrap();
        assert_eq!(label["type"], "label");
        assert_eq!(label["targetId"], "u1");
        assert_eq!(label["label"], "start");
        assert_eq!(label["id"].as_str().unwrap().len(), 8);

        let turns = history::turns(&branch.entries);
        assert_eq!(
            turns
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            ["100"]
        );
    }

    #[test]
    fn keeps_the_whole_branch_without_a_turn() {
        let entries = session();
        let whole = branch(&entries, "/s/old.jsonl", "new", "t", None).unwrap();
        let kept = ids(&whole);
        assert!(!kept.contains(&"x1"), "the abandoned branch is left out");
        assert!(kept.contains(&"a3"));
        let turns = history::turns(&whole.entries);
        assert_eq!(
            turns
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            history::turns(&entries)
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>()
        );
        let last = branch(&entries, "/s/old.jsonl", "new", "t", Some("105")).unwrap();
        assert_eq!(ids(&last).len(), kept.len());
    }

    #[test]
    fn refuses_unknown_turns_and_unlinked_sessions() {
        let entries = session();
        // The abandoned branch's prompt is not a turn of the active branch.
        assert_eq!(
            branch(&entries, "p", "n", "t", Some("104")).err(),
            Some(Refusal::NoTurn)
        );
        assert_eq!(
            branch(&entries, "p", "n", "t", Some("101")).err(),
            Some(Refusal::NoTurn)
        );
        let v1 = [json!({ "type": "session", "id": "old" })];
        assert_eq!(
            branch(&v1, "p", "n", "t", None).err(),
            Some(Refusal::Format)
        );
    }

    #[test]
    fn names_files_like_pi() {
        assert_eq!(
            file_name("2026-11-19T10:12:40.756Z", "abc"),
            "2026-11-19T10-12-40-756Z_abc.jsonl"
        );
    }
}
