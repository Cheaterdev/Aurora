# Aurora MCP endpoint

`POST /mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io)
(Streamable HTTP transport, JSON-RPC 2.0). It uses the same API keys as the
REST API ([api.md](api.md)) and calls the same store functions, so a tool does
what the matching REST call does, with the key's user as the actor of every
event.

## Connecting

URL `http://127.0.0.1:8787/mcp` (the address Aurora listens on). Create a key
for the user the client should act as (`POST /api/v1/keys`, or on the keys page;
an agent user for an agent, `read` scope for a client that only looks).

```bash
claude mcp add --transport http aurora http://127.0.0.1:8787/mcp \
  --header "Authorization: Bearer <key>"
```

Any other client needs the same two things: the URL and the header
`Authorization: Bearer <key>`.

## Protocol notes

* **Stateless.** No session: the server issues no `Mcp-Session-Id`, and every
  request is judged by its own key. `initialize` can be repeated; skipping it
  works too.
* **No SSE.** A request is answered with one `application/json` response. The
  server never starts a message, so `GET /mcp` and `DELETE /mcp` are `405`.
* **Messages.** A JSON-RPC request gets `200` and its response. A notification
  (`notifications/initialized`, `notifications/cancelled`, ...) or a response
  from the client gets `202` with no body. A batch (array, 1 to 50 messages) is
  answered with an array of the responses, or `202` if it held only
  notifications.
* **Methods.** `initialize`, `notifications/initialized`, `ping`, `tools/list`,
  `tools/call`. Anything else is `-32601`. The server offers tools only
  (`capabilities.tools.listChanged` is false).
* **Revisions.** Supported: `2024-11-05`, `2025-03-26`, `2025-06-18`. The
  server answers `initialize` with the newest supported revision that is not
  newer than the client's, or its latest (`2025-06-18`) when the client's is
  older than all of them. An `MCP-Protocol-Version` header, if sent, must be a
  supported revision (`400` otherwise).
* **Limits.** The body is at most 1 MiB (`413`).
* **Origin.** A request with an `Origin` header whose host is neither the
  request's `Host` nor `localhost`, `127.0.0.1` or `[::1]` is refused with
  `403` (DNS rebinding protection). Clients that are not browsers send none.

## Authentication and permissions

* `Authorization: Bearer <key>` only. Session cookies are not accepted. No,
  unknown, malformed, revoked or expired key: `401` with
  `WWW-Authenticate: Bearer` and `{"error": ...}`.
* The key's user is the actor, and the user's rights are the key's rights
  (comments may only be edited or deleted by their author or the owner).
* A `read` key may use the tools annotated `readOnlyHint: true`. Calling any
  other tool returns an error result saying "this key is read-only".
* Owner-only operations (creating users, managing other users' keys, creating
  projects) are not exposed over MCP; use the REST API.
* A key of a registered agent (see `list_agents`) is limited to the agent's
  allowed projects, an empty list meaning none. A tool call that names another
  project returns an error result `this agent may not access project KEY`;
  `list_projects` and `search` show only allowed projects. If the agent is
  disabled every request is `403` "agent is disabled". Humans and agent users
  without a registry entry are not restricted.

## Errors

* **Protocol errors** are JSON-RPC errors: `-32700` parse error (HTTP `400`),
  `-32600` invalid request (`400`, or `413` for an oversized body), `-32601`
  unknown method, `-32602` invalid params (no tool name, unknown tool name,
  `arguments` not an object, `initialize` without `protocolVersion`).
* **Tool errors** are normal results with `isError: true` and one text block
  that says what to fix, e.g. `invalid: ...`, `conflict: ...`, `not allowed: ...`,
  `... not found; check the project key, number or id`. A column that cannot be
  found lists the project's columns. Unknown arguments are errors too, so a
  misspelled field is never ignored silently.

## Results

A successful tool returns `content` with two text blocks (a short sentence, then
the JSON, for clients that ignore `structuredContent`), `structuredContent`
with the JSON, and `isError: false`. Times are Unix milliseconds. Lists are
bounded: descriptions are cut to 300 characters and comments to 2000
(`description_truncated` / `body_truncated` is then true; `get_issue` and
`get_document` return full text), and every list has a default page size and a
`next_offset` while more remain.

## Request ids (safe retries)

Every tool that changes data (the ones that are not `readOnlyHint`) accepts an
optional string argument `request_id`: 8 to 128 characters of `A-Z a-z 0-9 . _ : -`.
Call it again with the same `request_id` and the same arguments, for example
after a timeout, and the first result comes back instead of the change being
made twice. The repeated result has `_meta.idempotent_replay: true`; nothing is
changed and no event is written.

* The same `request_id` with other arguments (or for another tool) is an error
  result `invalid: request id was already used for a different request`. A
  malformed id is an error result too, and a read tool does not accept one.
* Only successful calls are remembered. A call that returned an error result
  changed nothing and can be repeated after the cause is fixed.
* An id belongs to the key's user and is shared with the REST API's
  `Idempotency-Key`; it is remembered for 7 days, and the id and the result are
  stored in the same transaction as the change, so a crash cannot apply a
  call twice. See "Idempotency" in [api.md](api.md) for the rules, the
  retention and the size limit (a result over 256 KiB is replayed as a note).

## Addressing

An issue is named by `issue: "TRK-12"` or by `project: "TRK"` plus `number: 12`.
In arguments that point at another issue (`parent`, `to`, `before`, `after`) a
bare number means an issue of the same project and `"TRK-12"` must name the same
project. A column is named by `column_id`, `column` (name, case-insensitive) or
`state_type` (the first column of that type in board order): give one. Labels,
issue types and assignees may be given by name where the schema says so.

## Tools

### `list_projects`

No arguments. `{"projects": [{id, key, name, created}]}`.

### `get_project`

`project` (key). The project, `columns` (with `state_type`), `issue_types`,
`labels` and `initiatives` (with progress). Call it first to learn the ids.

```json
{"name": "get_project", "arguments": {"project": "TRK"}}
```

### `list_issues`

`project`; optional `column_id` or `column`, `state_type`, `label_id` or `label`,
`assignee_id`, `parent` (issue number or `"none"`), `include_archived`,
`archived_only`, `limit` (25, max 100), `offset`. `{"issues", "total", "limit",
"offset", "next_offset"?}`.

```json
{"name": "list_issues", "arguments": {"project": "TRK", "state_type": "started"}}
```

### `get_issue`

`issue` or `project` + `number`. The issue with `children`, `links`
(`blocks`, `blocked_by`, `related`, each with `link_id`), `documents`,
`comments_count`, `label_names` and `column`.

### `create_issue`

`project`, `title`; optional `description`, `type_id` or `type` (needed unless
the project has one type), `column_id` / `column` / `state_type` (default first
backlog column), `assignee_id` or `assignee` (user name), `parent`, `initiative`
(id), `priority` (`urgent`, `high`, `medium`, `low`), `labels` (ids or names),
`request_id`. Returns the issue detail.

```json
{"name": "create_issue", "arguments": {"project": "TRK", "title": "Fix login", "priority": "high", "labels": ["bug"]}}
```

### `update_issue`

`issue` or `project` + `number`, and at least one of `title`, `description`,
`type_id` / `type`, `assignee_id` / `assignee`, `parent`, `initiative`,
`priority`, `labels` (replaces the set). Only what is passed changes; `null`
clears `description`, `assignee_id`, `assignee`, `parent`, `initiative` and
`priority`. The column is changed with `move_issue`. Optional `request_id`.

```json
{"name": "update_issue", "arguments": {"issue": "TRK-12", "priority": null, "assignee": "alice"}}
```

### `move_issue`

`issue` or `project` + `number`; one of `column_id`, `column`, `state_type`;
optionally one of `before` / `after` (issue numbers in the target column).
Without them the issue goes to the end of the column. Optional `request_id`.

```json
{"name": "move_issue", "arguments": {"issue": "TRK-12", "state_type": "completed"}}
```

### `archive_issue`

`issue` or `project` + `number`; optional `request_id`. Soft delete; the issue
keeps its number.

### `unarchive_issue`

`issue` or `project` + `number`; optional `request_id`. Restores an archived
issue.

### `add_comment`

`issue` or `project` + `number`, `body` (plain text, 1 to 20000 characters),
optional `parent` (id of a top-level comment, to reply) and `request_id`.
`@name` mentions a user
and emits `mention.created`, which an agent user's runner is woken by.

```json
{"name": "add_comment", "arguments": {"issue": "TRK-12", "body": "Done, @alice please review"}}
```

### `list_comments`

`issue` or `project` + `number`; `limit` (30, max 100) and `offset` count
top-level comments; replies come with their comment. Oldest first.

### `edit_comment`

`comment_id`, `body`, and `project` or `issue` to name the project; optional
`request_id`. Author or owner only.

### `delete_comment`

`comment_id` and `project` or `issue`; optional `request_id`. Author or owner
only; replies stay.

### `link_issues`

`issue` or `project` + `number`, `to` (number or `"KEY-n"`), `kind`:
`blocks` (this blocks `to`), `blocked_by` (`to` blocks this) or `relates`.
Optional `request_id`. A duplicate link or a blocking cycle is an error result.

```json
{"name": "link_issues", "arguments": {"issue": "TRK-12", "to": "TRK-13", "kind": "blocks"}}
```

### `unlink_issues`

`link_id` (from `get_issue`) and `project` or `issue`; optional `request_id`.

### `search`

`q`; optional `project`, `kinds` (any of `issue`, `comment`, `document`),
`include_archived`, `limit` (10, max 50), `offset`. Hits carry the kind,
project, issue or document, a plain-text `title` and `snippet`. An identifier
like `TRK-12` is found directly (`exact: true`).

### `list_documents`

`project`; optional `include_archived`, `limit` (100, max 500), `offset`. Tree
order, each with `depth`, no bodies.

### `get_document`

`project`, `id`; optional `max_body_chars` (50000). The document with
`revision`, `ancestors`, `children`, `links`, and `body_chars`.

### `create_document`

`project`, `title`; optional `body` (Markdown), `parent` (document id),
`request_id`.

### `update_document`

`project`, `id`, at least one of `title`, `body`, `parent` (`null`: top level);
pass `base_revision` (from `get_document`) to get a `conflict` error instead of
overwriting a newer edit. Optional `request_id`.

```json
{"name": "update_document", "arguments": {"project": "TRK", "id": 3, "body": "# New", "base_revision": 2}}
```

### `list_initiatives`

`project`; optional `status`. Initiatives with progress and health.

### `list_agents`

No arguments. Read only. The registered agents with their trigger switches,
limits and allowed projects (a registered agent sees only the allowed ones
among them); the command, arguments and instructions are never included.

### `get_current_user`

No arguments. The key's user, its role and the key's scope.

## For agents

An agent connects with the key of its agent user and may do everything the
tools above allow with the user's rights: read projects, issues, documents and
search; create, change, move, archive and link issues; comment; write
documents. It cannot create users, keys or projects, nor administer anything
(use the REST API with the owner's key). Per the tracker's rules, an agent
moves its issue to a `started` column before working and to a `completed` one
when done, using `move_issue` with `state_type`.
