Captured with Claude Code 2.1.289 through keel-tester on 2026-10-05, using
`claude-sonnet-4-6`, medium effort, standard speed, in a scratch directory.
A transparent stdin/stdout proxy recorded the existing stream-json protocol.

The root prompts requested `claude-ok`, a single child returning `child-ok`,
a child asking its own child for `nested-ok`, and a background Bash command
`sleep 12; printf claude-task-ok`. All three agents actually used Sonnet 4.6.
The command finished after the root result; Claude subsequently read its output.
No permission controls were changed by this checkpoint.

The fixture selects initialize, user input, complete assistant/tool-result,
result, and task system frames in their original order. Partial stream frames,
initialization responses, status, rate-limit and command lifecycle frames are
omitted. IDs and output paths are anonymized consistently; thinking blocks and
signatures are removed. This is a protocol/mapping regression fixture, not a
complete process replay or proof of consumer-facing observation support.

Native ancestry is established by joining task_started.tool_use_id with the
owning assistant frame's parent_tool_use_id, not by guessing from spawn_depth.
The installed CLI emits background_tasks_changed.tasks as task objects; the
older installed SDK's ID-array description should not override captured traffic.

The forwarding request follows the official SDK's initialize field
`forwardSubagentText`. Relevant official documentation:
https://code.claude.com/docs/en/agent-sdk/subagents
https://code.claude.com/docs/en/sub-agents

Next checkpoints: attributed records and recursive agent trees; background task
state/output; native stop_task controls; snapshots, recovery, and stored history.
