//! Read-only native descendant discovery and membership-scoped history.
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{Codex, provider, types};
use crate::{Error, Result, agent, turn};
use provider::PAGE_LIMIT;

const SOURCES: [&str; 5] = [
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
];

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cursor {
    root: String,
    parent: Option<String>,
    ancestor: Option<String>,
    agent: Option<String>,
    native: Option<String>,
    root_done: bool,
}

fn cursor_error(value: &str) -> Error {
    Error::InvalidSetting {
        setting: "cursor",
        value: value.into(),
        reason: "Invalid or differently scoped observation cursor".into(),
    }
}
fn decode(value: &str) -> Result<Cursor> {
    serde_json::from_str(
        value
            .strip_prefix("keel:v1:")
            .ok_or_else(|| cursor_error(value))?,
    )
    .map_err(|_| cursor_error(value))
}
fn encode(cursor: &Cursor) -> String {
    format!(
        "keel:v1:{}",
        serde_json::to_string(cursor).expect("cursor serialization")
    )
}
fn limit(limit: Option<u32>) -> Result<u32> {
    if limit == Some(0) {
        return Err(Error::InvalidSetting {
            setting: "limit",
            value: "0".into(),
            reason: "Must be positive".into(),
        });
    }
    Ok(limit.unwrap_or(PAGE_LIMIT).min(PAGE_LIMIT))
}

pub(super) fn parent(thread: &types::Thread) -> Option<String> {
    thread.parent_thread_id.clone().or_else(|| {
        thread.source["subAgent"]["thread_spawn"]["parent_thread_id"]
            .as_str()
            .map(str::to_owned)
    })
}

pub(super) fn summary(thread: &types::Thread, root: &str) -> agent::Summary {
    let spawn = &thread.source["subAgent"]["thread_spawn"];
    agent::Summary {
        id: thread.id.clone(),
        parent: if thread.id == root {
            agent::Parent::Root
        } else {
            parent(thread)
                .map(|id| agent::Parent::Agent { id })
                .unwrap_or(agent::Parent::Unknown)
        },
        name: thread
            .agent_nickname
            .clone()
            .or_else(|| spawn["agent_nickname"].as_str().map(str::to_owned))
            .or(thread.name.clone()),
        role: thread
            .agent_role
            .clone()
            .or_else(|| spawn["agent_role"].as_str().map(str::to_owned)),
        model: thread.model.clone(),
        description: (!thread.preview.is_empty()).then(|| thread.preview.clone()),
        background: None,
        status: if matches!(thread.status, types::ThreadStatus::Active) {
            agent::Status::Running
        } else {
            agent::Status::Unknown
        },
        origin: None,
        created_at: (thread.created_at != 0).then_some(thread.created_at),
        updated_at: (thread.updated_at != 0).then_some(thread.updated_at),
    }
}

impl Codex {
    async fn agent_thread(&self, id: &str) -> Result<types::Thread> {
        let response: types::ThreadReadResponse = self
            .request("thread/read", &types::ThreadIdParams { thread_id: id })
            .await
            .map_err(|error| provider::unknown_session(id, error))?;
        Ok(response.thread)
    }

    async fn membership(&self, root: &str, id: &str) -> Result<()> {
        if id == root {
            return Ok(());
        }
        let original = id.to_owned();
        let mut current = id.to_owned();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            let thread = match self.agent_thread(&current).await {
                Ok(thread) => thread,
                Err(Error::SessionNotFound { .. }) => break,
                Err(error) => return Err(error),
            };
            let Some(parent) = parent(&thread) else {
                break;
            };
            if parent == root {
                return Ok(());
            }
            current = parent;
        }
        Err(Error::AgentNotFound {
            session_id: root.into(),
            id: original,
        })
    }

    pub async fn list_agents(&self, root: &str, query: agent::Query) -> Result<agent::Page> {
        query.validate()?;
        let root_thread = self.agent_thread(root).await?;
        if let Some(filter) = query.parent_id.as_deref().or(query.ancestor_id.as_deref()) {
            self.membership(root, filter).await?;
        }
        let mut cursor = match query.cursor.as_deref() {
            Some(value) => {
                let cursor = decode(value)?;
                if cursor.root != root
                    || cursor.parent != query.parent_id
                    || cursor.ancestor != query.ancestor_id
                    || cursor.agent.is_some()
                {
                    return Err(cursor_error(value));
                }
                cursor
            }
            None => Cursor {
                root: root.into(),
                parent: query.parent_id.clone(),
                ancestor: query.ancestor_id.clone(),
                agent: None,
                native: None,
                root_done: false,
            },
        };
        let limit = limit(query.limit)?;
        let mut agents = vec![];
        if !cursor.root_done && query.parent_id.is_none() && query.ancestor_id.is_none() {
            agents.push(summary(&root_thread, root));
        }
        cursor.root_done = true;
        let remaining = limit - agents.len() as u32;
        if remaining == 0 {
            return Ok(agent::Page {
                agents,
                next_cursor: Some(encode(&cursor)),
            });
        }
        let response: types::ThreadListResponse = self
            .request(
                "thread/list",
                &types::ThreadListParams {
                    cursor: cursor.native.take(),
                    limit: Some(remaining),
                    sort_key: "created_at",
                    sort_direction: "asc",
                    cwd: None,
                    source_kinds: SOURCES.to_vec(),
                    parent_thread_id: query.parent_id,
                    ancestor_thread_id: if cursor.parent.is_some() {
                        None
                    } else {
                        Some(query.ancestor_id.unwrap_or_else(|| root.into()))
                    },
                },
            )
            .await?;
        agents.extend(response.data.iter().map(|thread| summary(thread, root)));
        cursor.native = response.next_cursor;
        let next_cursor = cursor.native.as_ref().map(|_| encode(&cursor));
        Ok(agent::Page {
            agents,
            next_cursor,
        })
    }

    pub async fn list_agent_turns(
        &self,
        root: &str,
        id: &str,
        query: turn::Query,
    ) -> Result<agent::TurnPage> {
        self.agent_thread(root).await?;
        self.membership(root, id).await?;
        let cursor = if let Some(value) = query.cursor.as_deref() {
            let cursor = decode(value)?;
            if cursor.root != root
                || cursor.agent.as_deref() != Some(id)
                || cursor.parent.is_some()
                || cursor.ancestor.is_some()
            {
                return Err(cursor_error(value));
            }
            cursor.native
        } else {
            None
        };
        let page = self
            .list_turns(
                id,
                turn::Query {
                    cursor,
                    limit: Some(limit(query.limit)?),
                },
            )
            .await?;
        let next_cursor = page.next_cursor.map(|native| {
            encode(&Cursor {
                root: root.into(),
                parent: None,
                ancestor: None,
                agent: Some(id.into()),
                native: Some(native),
                root_done: true,
            })
        });
        Ok(agent::TurnPage {
            turns: page
                .turns
                .into_iter()
                .map(|turn| agent::OwnedTurn {
                    agent_id: id.into(),
                    turn,
                })
                .collect(),
            next_cursor,
        })
    }

    pub(super) async fn discover_stored(
        &self,
        connection: &super::connection::AppServer,
        root: &str,
    ) -> Result<()> {
        let mut cursor = None;
        let mut seen = HashSet::new();
        for _ in 0..provider::MAX_PAGES {
            let response: types::ThreadListResponse = connection
                .request(
                    "thread/list",
                    &types::ThreadListParams {
                        cursor,
                        limit: Some(PAGE_LIMIT),
                        sort_key: "created_at",
                        sort_direction: "asc",
                        cwd: None,
                        source_kinds: SOURCES.to_vec(),
                        parent_thread_id: None,
                        ancestor_thread_id: Some(root.into()),
                    },
                    self.config().request_timeout,
                )
                .await?;
            for thread in response.data {
                connection.stored_agent(thread);
            }
            match response.next_cursor {
                None => return Ok(()),
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Err(cursor_error("repeated stored-discovery cursor")),
            }
        }
        Err(cursor_error("stored-discovery page limit exceeded"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_cursor_roundtrips_and_rejects_native_or_malformed_values() {
        let cursor = Cursor {
            root: "root".into(),
            parent: None,
            ancestor: Some("child".into()),
            agent: None,
            native: Some("native".into()),
            root_done: true,
        };
        let decoded = decode(&encode(&cursor)).unwrap();
        assert_eq!(decoded.root, "root");
        assert_eq!(decoded.ancestor.as_deref(), Some("child"));
        assert!(decode("native").is_err());
        assert!(decode("keel:v1:{}").is_err());
        assert!(limit(Some(0)).is_err());
    }
}
