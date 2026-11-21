# Aurora REST API

Everything the web pages do is also available as JSON under `/api/v1`. The
agent-facing counterpart is the MCP endpoint, see [mcp.md](mcp.md). The route
table below is checked against the router by a test
(`crates/aurora/tests/api_routes.rs`): a route that is registered but not
documented here, or documented but not registered, fails the build. A running
server lists the same table at `GET /api/v1`.

## Authentication

Every endpoint except `POST /api/v1/setup` and `POST /api/v1/auth/sign-in`
needs credentials. Two ways to present them; if an `Authorization` header is
present only it is considered.

* **API key**: `Authorization: Bearer aur_...`. Keys belong to a user (a human
  or an agent user), are shown once when created or rotated, and have a
  scope:
  * `write` (the default) may use every method the user may use.
  * `read` may only use `GET` and `HEAD`; anything else is `403`.
* **Session cookie**: `aurora_session`, set by `POST /api/v1/auth/sign-in`
  (`HttpOnly`, `SameSite=Lax`). A request that is not `GET`/`HEAD` and
  authenticates with the cookie must also carry `X-Requested-With: aurora`
  (CSRF protection), otherwise `403`. Bearer requests do not need it.

Missing, malformed, unknown, revoked or expired credentials: `401` with
`WWW-Authenticate: Bearer`.

Permissions are those of the key's user. Humans have no per-project
permissions: any user who may write may change any project, comment, link or
document. What is restricted: listing and creating users and managing other
users' keys are owner only; editing or deleting a comment is for its author
and the owner. Every change records the key's user as the actor of its event.
A key can only act as its own user; there is no way to act as another one.

**Registered agents are restricted.** An agent user that has an entry in the
agent registry (`/api/v1/agents`) may only use the projects on its allow-list:

* Any route under `/api/v1/projects/{key}` for another project (or an unknown
  one: the answer is the same) is `403 {"error": "this agent may not access
  project KEY"}`.
* `GET /api/v1/projects` and `GET /api/v1/search` list only allowed projects;
  `GET /api/v1/events` only their events (events without a project are not
  shown); naming another project in `project` is the `403` above.
* `POST /api/v1/projects`, the consumers endpoints and the live stream
  `/events/stream` are `403` for it. The web pages are `403` for its key.
* If the agent is disabled, every call is `403 {"error": "agent is disabled"}`
  except `GET /api/v1/auth/me`.

Humans, and agent users without a registry entry, are not restricted by any
of this. Deleting the entry lifts the restriction, and an agent user created
with `POST /api/v1/users` stays unrestricted until it is registered.
`GET /api/v1/auth/me` has `"agent": null` or `{id, enabled, projects}`.

## Conventions

* Requests with a body are JSON with `Content-Type: application/json`.
  Unknown fields are rejected.
* Errors are `{"error": "<what to fix>"}` with a status code:
  `400` bad request, `401` no valid credentials, `403` not allowed,
  `404` `"<thing> not found"`, `409` conflict (duplicate, cycle, a document
  edited since `base_revision`, ...), `415` missing JSON content type,
  `422` invalid body, query or values, `500` `"internal server error"`.
* `201` for creates, `200` with a body for reads and updates, `204` with no
  body for deletes.
* Times are Unix milliseconds (`created`, `updated`, ...). Initiative target
  dates are calendar dates `YYYY-MM-DD`.
* Projects are addressed by their key (`TRK`), issues by project key and
  number (`/projects/TRK/issues/12`, identifier `TRK-12`), everything else
  (columns, labels, comments, links, initiatives, documents, keys) by numeric
  `id`. Fields named `..._id` are ids, `parent` on an issue is an issue number,
  `parent` on a document is a document id.
* A `PATCH` changes only the fields present; `null` clears the fields that
  can be cleared (said per endpoint). A write that changes nothing writes
  nothing and emits no event.
* Pagination: `limit` and `offset` where listed. A `limit` above the maximum
  is clamped; the response says the `limit`, `offset` and `total` used.
  Event streams page with a cursor (`after` / consumers).
* Every change and its event are committed in one transaction.
* Changing requests can be made safe to repeat with a request id, see
  Idempotency.

## Idempotency

A client that does not know whether a change was applied (a timeout, a lost
connection, a crash on either side) can send the same request again without
applying it twice.

* **Header.** `Idempotency-Key: <id>` on a `POST`, `PUT`, `PATCH` or `DELETE`.
  The id is 8 to 128 characters of `A-Z a-z 0-9 . _ : -` (`422` otherwise);
  generate a fresh one per intended change, for example a UUID. It is optional:
  a request without one behaves exactly as it always did. On a read the header
  is ignored. The MCP endpoint takes the same thing as the argument
  `request_id` of its write tools (see [mcp.md](mcp.md)); both share one log.
* **Repeating.** The same id with the same request (method, path and query,
  and the JSON body compared by value, so key order and spacing do not matter)
  returns the original status and body again with the response header
  `Idempotent-Replay: true`. Nothing is changed and no event is written; a
  `204` stays `204`. The first response has no such header.
* **Different request.** The same id with another method, path, query or body
  is `422 {"error": "request id was already used for a different request"}`
  and changes nothing.
* **Whose id.** An id belongs to the user of the credentials: two users may use
  the same id independently. A replay still needs valid credentials, but the
  user's rights are not evaluated again; the stored answer is returned.
* **Crash safety.** The id and the response are written in the same database
  transaction as the change and its event, so after a crash either all of
  them are there or none: a retry then applies the change once. Two requests
  with the same id sent at the same time are applied one after the other; the
  second one is a replay.
* **What is remembered.** Only successful (`2xx`) results. A request that
  failed (`4xx`, `5xx`) changed nothing and leaves no trace, so a retry after
  fixing the request, or after a transient failure, runs again. A request
  body is read in full before it is handled when the header is present, at
  most 2 MiB (`413`).
* **Retention.** An id is remembered for 7 days; a repeat after that is a new
  request and is applied. Expired ids are deleted in batches of at most 500 at
  start-up and then every hour, never in the path of a user's write. The period
  can be changed with `--request-retention-days <n>` (env
  `AURORA_REQUEST_RETENTION_DAYS`, 1 to 3650); it is not in the usage text.
* **Large responses.** A response larger than 256 KiB is not stored. The id is
  still remembered, and a repeat returns the original status with
  `{"replayed": true, "note": "original response too large to replay"}` and
  `Idempotent-Replay: true`; read the object again with a `GET`.
* **One-time keys.** `POST /keys`, `POST /keys/{id}/rotate` and `POST /agents`
  with `"create_key": true` return a secret that is shown once. The secret is
  never stored, so a replay returns the same body with `"key": null`,
  `"key_replayed": true` and a `"message"` saying that the key was shown once
  (the listed `prefix` is omitted too; no text that looks like a key is kept).
  If the first response was lost, rotate the key (or create another) with a new
  id.
* **Not covered.** `POST /setup` and `POST /auth/sign-in` (public, no id),
  `POST /auth/sign-out` and `POST /projects/{key}/documents/preview` (nothing to
  repeat): the header is ignored there, even a malformed one. The web pages'
  forms use session cookies and do not take ids.

## Index

### `GET /api/v1`

The route table: `{"name", "version", "docs", "mcp", "endpoints": [{"method",
"path", "public"}]}`; paths are relative to `/api/v1`.

## Authentication endpoints

### `POST /api/v1/setup`

First run only, no credentials. Body `{"name", "password"}` (password at
least 10 characters). `201` with the owner user `{id, name, kind, role,
disabled}`. `409` once an owner exists.

### `POST /api/v1/auth/sign-in`

No credentials. Body `{"name", "password"}`. `200` with the user and a
`Set-Cookie: aurora_session=...`. `401` `"invalid name or password"`.

### `POST /api/v1/auth/sign-out`

Ends the cookie session if there is one and clears the cookie. `204`.

### `GET /api/v1/auth/me`

`{"user": {...}, "scope": "read"|"write", "via": "key"|"session", "agent":
null | {"id", "enabled", "projects"}}`. `agent` is set for a registered agent
user; this is the one call a disabled agent may make.

## Keys and users

### `GET /api/v1/keys`

Query `user_id` (default: you; other users only for the owner). Array of keys
`{id, user_id, name, prefix, scope, created, last_used, revoked}`; never the
secret. `403` for another user's keys.

### `POST /api/v1/keys`

Body `{"name", "scope"?: "read"|"write", "user_id"?}` (`user_id` other than
your own: owner only). `201` with the key plus its one-time `"key"` secret.
`409` for a disabled user.

### `DELETE /api/v1/keys/{id}`

Revokes the key (idempotent). `204`. `404`, `403` for another user's key.

### `POST /api/v1/keys/{id}/rotate`

Replaces the secret of a live key in place; the old secret stops working.
`200` with the key and the new `"key"`. `409` for a revoked key.

### `GET /api/v1/users`

Owner only (`403` otherwise). Array of users, by id.

### `POST /api/v1/users`

Owner only. Body `{"name"}`. Creates an agent user (no password; it acts
through its keys). `201` with the user. `409` if the name is taken.

## Projects

A project is `{id, key, name, created}`.

### `GET /api/v1/projects`

Array of projects.

### `POST /api/v1/projects`

Body `{"key", "name", "columns"?: [{"name", "state_type"}]}`. Without
`columns` the project gets Backlog, Todo, In Progress, Done and Canceled. A
state type is `backlog`, `unstarted`, `started`, `completed` or `canceled`.
`201` with the project and its `columns`. `409` for a duplicate key, `422`
for an invalid key or name.

### `GET /api/v1/projects/{key}`

The project.

### `PATCH /api/v1/projects/{key}`

Body `{"name"}`. `200` with the project.

### `GET /api/v1/projects/{key}/columns`

Query `state_type`. Columns `{id, project_id, name, state_type, position}` in
board order.

### `POST /api/v1/projects/{key}/columns`

Body `{"name", "state_type"}`. `201` with the column, added last.

### `PUT /api/v1/projects/{key}/columns/order`

Body `{"ids": [column ids]}` listing every column once. `200` with the columns
in the new order.

### `PATCH /api/v1/projects/{key}/columns/{id}`

Body `{"name"}`. `200` with the column.

### `DELETE /api/v1/projects/{key}/columns/{id}`

`204`. `409` while any issue, archived ones included, is in the column.

### `GET /api/v1/projects/{key}/issue-types`

Array of `{id, project_id, name}`.

### `POST /api/v1/projects/{key}/issue-types`

Body `{"name"}`. `201`.

### `PATCH /api/v1/projects/{key}/issue-types/{id}`

Body `{"name"}`. `200`.

### `DELETE /api/v1/projects/{key}/issue-types/{id}`

`204`. `409` while issues use the type.

### `GET /api/v1/projects/{key}/labels`

Array of `{id, project_id, name, color}`.

### `POST /api/v1/projects/{key}/labels`

Body `{"name", "color"?}` (color `#rrggbb`, default `#808080`). `201`.

### `PATCH /api/v1/projects/{key}/labels/{id}`

Body `{"name"?, "color"?}`, at least one. Applied in one transaction. `200`.

### `DELETE /api/v1/projects/{key}/labels/{id}`

`204`; the label is removed from its issues.

## Issues

An issue is `{id, project_id, number, identifier, type_id, column_id, title,
description, assignee_id, parent, initiative_id, priority, position, created,
updated, archived, labels, blocked, blocker_count}`. `parent` is an issue
number; `labels` are label ids; `priority` is `urgent`, `high`, `medium`,
`low` or `null`; `blocked` is true while an open blocker exists.

### `GET /api/v1/projects/{key}/issues`

Query (all optional): `column_id`, `state_type`, `label_id`, `assignee_id`,
`parent` (an issue number, or `none` for top-level issues), `include_archived`,
`archived_only`, `limit` (default 50, max 200), `offset`. Response
`{"issues": [...], "total", "limit", "offset"}`, ordered by column position
then issue position.

### `POST /api/v1/projects/{key}/issues`

Body `{"title", "description"?, "type_id"?, "column_id"?, "assignee_id"?,
"parent"?, "initiative"?, "priority"?, "labels"?: [label ids]}`. `type_id` is
required unless the project has exactly one issue type. `column_id` defaults to
the first backlog column. Title at most 300 characters, description at most
50000. Parents nest at most 8 deep. `201` with the issue; `422` for ids of
another project or invalid values.

### `GET /api/v1/projects/{key}/issues/{number}`

The issue plus `children` (`{number, title, column_id, archived}`), `links`
(`{blocks, blocked_by, related}`, see links below) and `documents`
(`{id, title, link_id}`).

### `PATCH /api/v1/projects/{key}/issues/{number}`

Body, any of: `title`, `description`, `type_id`, `assignee_id`, `parent`,
`initiative` (initiative id), `priority`, `labels` (the full new set). `null`
clears `description`, `assignee_id`, `parent`, `initiative` and `priority`.
The column is changed with `move`. `200` with the issue detail. `409` for a
parent cycle.

### `POST /api/v1/projects/{key}/issues/{number}/move`

Body `{"column_id", "before"?, "after"?, "position"?}`; at most one of the
last three; `before` / `after` are issue numbers in the target column; none
puts the issue at the end. `200` with the issue detail.

### `POST /api/v1/projects/{key}/issues/{number}/archive`

Soft delete. `200` with the issue detail.

### `POST /api/v1/projects/{key}/issues/{number}/unarchive`

`200` with the issue detail.

### `GET /api/v1/projects/{key}/issues/{number}/events`

Query `limit` (default 50, at most 500). `{"events": [...]}`: the newest
events about the issue (changes, comments, links), oldest first. Event shape:
see Events.

## Links

Issues of one project are linked by `blocks` (directed) or `relates`
(symmetric). A link is `{id, kind, from: {number, identifier}, to: {...},
created, created_by}`. A linked issue as listed is `{link_id, number,
identifier, title, column_id, state_type, archived}`.

### `GET /api/v1/projects/{key}/issues/{number}/links`

`{"blocks": [...], "blocked_by": [...], "related": [...]}`.

### `POST /api/v1/projects/{key}/issues/{number}/links`

Body `{"kind": "blocks"|"relates", "to": number | "KEY-n", "direction"?:
"blocks"|"blocked_by"}`. With `direction: "blocked_by"` (kind `blocks` only)
`to` blocks this issue. `201` with the link. `409` for a duplicate or a
`blocks` cycle, `422` for the same issue, another project or an unknown
`to`.

### `DELETE /api/v1/projects/{key}/links/{id}`

`204`.

## Comments

A comment is `{id, issue: {number, identifier}, parent, author: {id, name,
kind}, body, created, edited, deleted, mentions: [{user_id, name}], replies}`.
Bodies are plain text, 1 to 20000 characters. `@name` mentions an existing
user and emits `mention.created`. Replies nest one level.

### `GET /api/v1/projects/{key}/issues/{number}/comments`

Top-level comments in creation order, each with its `replies`.

### `POST /api/v1/projects/{key}/issues/{number}/comments`

Body `{"body", "parent"?: comment id}`. `201` with the comment. `422` when
`parent` is not a live top-level comment of the issue.

### `GET /api/v1/projects/{key}/comments/{id}`

One comment (a deleted comment has an empty body and `deleted` set).

### `PATCH /api/v1/projects/{key}/comments/{id}`

Body `{"body"}`. Author or owner only (`403`). `200`.

### `DELETE /api/v1/projects/{key}/comments/{id}`

Soft delete. Author or owner only. `204`.

## Initiatives

An initiative is `{id, project_id, name, description, status, target_date,
created, updated, progress, health}`; status is `planned`, `active`, `paused`,
`completed` or `canceled`; `progress` is `{total, completed, canceled,
started, open, done, done_ratio, percent}`; `health` is `on_track`,
`at_risk`, `off_track` or `none`.

### `GET /api/v1/projects/{key}/initiatives`

Query `status`. Array of initiatives with progress and health.

### `POST /api/v1/projects/{key}/initiatives`

Body `{"name", "description"?, "status"?, "target_date"?}`. `201`.

### `GET /api/v1/projects/{key}/initiatives/{id}`

Query `limit`, `offset` for the issue page. The initiative plus `issues`
(`{number, identifier, title, column_id, state_type}`), `documents`, `total`,
`limit`, `offset`.

### `PATCH /api/v1/projects/{key}/initiatives/{id}`

Body, any of `name`, `description`, `status`, `target_date`; `null` clears the
description and the target date. `200`.

### `DELETE /api/v1/projects/{key}/initiatives/{id}`

`204`; its issues stay and are unassigned from it (one `issue.updated` event each).

## Documents

Markdown documents in a tree, with revisions. A document is `{id, project_id,
parent, title, body, position, revision, created, updated, created_by,
updated_by, archived}`; the detail adds `ancestors`, `children` (`{id,
title}`) and `links` (`{issues, initiatives}`). Titles are at most 200
characters, bodies at most 200000.

### `GET /api/v1/projects/{key}/documents`

Query `include_archived`. Flat list without bodies:
`{id, parent, title, position, revision, updated, archived}`.

### `POST /api/v1/projects/{key}/documents`

Body `{"title", "body"?, "parent"?: document id}`. `201` with the detail.

### `POST /api/v1/projects/{key}/documents/preview`

Body `{"body"}`. `{"html": "..."}`, rendered like the document page. Changes
nothing.

### `GET /api/v1/projects/{key}/documents/{id}`

The detail.

### `PATCH /api/v1/projects/{key}/documents/{id}`

Body, any of `title`, `body`, `parent` (`null`: top-level), `base_revision`.
A `base_revision` that is not the current revision is `409` (someone edited
meanwhile; read again and merge). Each change of title or body makes a new
revision.

### `POST /api/v1/projects/{key}/documents/{id}/move`

Body `{"parent"?, "before"?, "after"?, "position"?}`; `parent: null` means
top-level, absent keeps the parent; at most one of the last three (document
ids among the new siblings).

### `POST /api/v1/projects/{key}/documents/{id}/archive`

`200` with the detail.

### `POST /api/v1/projects/{key}/documents/{id}/unarchive`

`200` with the detail.

### `GET /api/v1/projects/{key}/documents/{id}/revisions`

Array of `{number, title, author_id, author, created, note, chars}`.

### `GET /api/v1/projects/{key}/documents/{id}/revisions/{number}`

`{number, title, body, author_id, author, created, note}`.

### `POST /api/v1/projects/{key}/documents/{id}/restore`

Body `{"revision", "base_revision"?}`. Makes the old revision current as a new
revision. `200` with the detail.

### `POST /api/v1/projects/{key}/documents/{id}/links`

Body: exactly one of `{"issue": number}` or `{"initiative": id}`. `201` with
`{id, document_id, kind, issue, initiative}`. `409` if already linked.

### `DELETE /api/v1/projects/{key}/documents/{id}/links/{link_id}`

`204`.

## Agents

The agent registry describes how an agent runs and what it may reach. It
holds no secrets: an agent's credentials are its keys, which stay in the keys
endpoints. An agent is `{id, user_id, name, description, on_assign,
on_mention, enabled, max_concurrent, timeout_seconds, projects, created,
updated}` plus, for the owner only, `{command, args, model, instructions,
work_dir}`. Everyone else gets the first set (the command, arguments,
instructions and working directory are redacted). `projects` is the sorted
list of allowed project keys.

**An agent with no allowed project can access no project at all.** The empty
list is not "all projects". See Authentication for the effect on the agent's
key.

Limits: `command` 1-500 characters, at most 50 `args` of at most 2000
characters, `instructions` at most 20000, `work_dir` (absolute path) at most
1000, `description` at most 1000, `model` at most 200, `max_concurrent` 1-16
(default 1), `timeout_seconds` 60-86400 (default 3600). `on_assign` and
`on_mention` default to true, `enabled` to true.

### `GET /api/v1/agents`

Array of agents by id, redacted unless you are the owner. A registered agent
sees only its allowed projects in each `projects` list.

### `POST /api/v1/agents`

Owner only (`403` otherwise). Body: `name` (creates the agent user in the same
transaction) or `user_id` (registers an existing agent user, once), `command`,
and optionally `description`, `args`, `model`, `instructions`, `work_dir`,
`max_concurrent`, `timeout_seconds`, `on_assign`, `on_mention`, `enabled`,
`projects`, and `create_key`. `201` with the agent. With `"create_key": true`
the response is `{"agent": {...}, "key": "aur_...", "key_info": {...}}`: a
`write`-scope key named "agent key", shown once, created in the same
transaction (`Cache-Control: no-store`). `409` for a taken name or an already
registered user, `404` for an unknown `user_id`, `422` for an invalid field,
an unknown project key in `projects`, or a user that is not an agent.

### `GET /api/v1/agents/{id}`

One agent, redacted unless you are the owner. `404`.

### `PATCH /api/v1/agents/{id}`

Owner only. Any of the create fields except `name`, `user_id` and
`create_key`; only the fields present change. `model` and `work_dir` accept
`null` to clear. `projects` replaces the whole set. `200` with the agent. A
patch that changes nothing writes nothing and emits no event.

### `DELETE /api/v1/agents/{id}`

Owner only. Removes the registry entry only: the agent user and its keys stay,
and the user is no longer restricted (see Authentication). `204`.

### `POST /api/v1/agents/{id}/enable`

Owner only. `200` with the agent; repeating it changes nothing.

### `POST /api/v1/agents/{id}/disable`

Owner only. `200` with the agent. The agent's keys then get `403` "agent is
disabled" on everything except `GET /api/v1/auth/me`, and no run is started
for it. Repeating it changes nothing.

## Search

### `GET /api/v1/search`

Query: `q`, `project` (key), `kind` (comma separated `issue`, `comment`,
`document`), `include_archived`, `limit` (default 20, max 100), `offset`.
`{"query", "total", "total_capped", "limit", "offset", "hits": [...]}`. A hit
has `kind`, `project`, `issue` (`{number, identifier, title}`), `comment_id`,
`document` (`{id, title}`), `title` and `snippet` (lists of `{text, matched}`
segments), `archived`, `exact`, `rank`. An identifier such as `TRK-12` finds
the issue exactly.

## Events and consumers

Every change appends an event `{id, project_id, project, kind, entity_kind,
entity_id, actor, payload, created}`; `actor` is the user id of the key or
session that made the change. Kinds: `project.*`, `column.*`, `issue_type.*`,
`label.*`, `issue.created|updated|moved|archived|unarchived`,
`link.added|removed`, `comment.created|edited|deleted`, `mention.created`,
`initiative.*`, `document.*`, `key.*`, `user.created`,
`agent.created|updated|deleted` (no project; the payload never carries the
command, arguments, instructions or working directory). Delivery to consumers
is at least once: tolerate repeats.

### `GET /api/v1/events`

Query `after` (event id, default 0), `limit` (default 100, max 500),
`project`, `entity_kind`, `kind_prefix` (e.g. `issue.`). `{"events": [...]}`
oldest first.

### `GET /api/v1/consumers`

Array of cursors `{name, last_event_id, updated}`.

### `PUT /api/v1/consumers/{name}`

Body `{"start": "beginning"|"latest"}`. Registers a named cursor (name: 1-100
of ASCII letters, digits, `.`, `_`, `-`). `201` with the cursor when new,
`200` when it existed (unchanged).

### `DELETE /api/v1/consumers/{name}`

`204`; `404` for an unknown consumer.

### `GET /api/v1/consumers/{name}/events`

Query `limit`, `project`, `entity_kind`, `kind_prefix`. `{"consumer": cursor,
"events": [...]}`: events after the cursor; the cursor does not move.

### `POST /api/v1/consumers/{name}/ack`

Body `{"up_to": event id}`. Moves the cursor forward (an id at or behind the cursor changes nothing).
`200` with the cursor; `422` for an event that does not exist yet.
