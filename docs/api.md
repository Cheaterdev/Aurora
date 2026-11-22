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
  `POST /auth/sign-out`, `POST /projects/{key}/documents/preview` and
  `POST /agents/{id}/prompt-preview` (nothing to repeat): the header is ignored
  there, even a malformed one. The web pages'
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

A project is `{id, key, name, created, rules}`. `rules` is the project's own
rules text for agents (empty when there is none; see Prompt templates).

Owners and members also see `repo_path` and `base_branch` (both `null` until
set): the git repository the project's runs take their working copies from
(see Runs). A registered agent's key never gets these two fields, and no MCP
tool returns them: the repository's folder is not for agents.

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

Body: `name`, `rules`, `repo_path` and/or `base_branch`; only the fields
present change, and at least one is required (`422` otherwise). `rules` is free
text of at most 10000 characters that the default prompt template shows to
agents under "Project rules"; `null` or blank clears it. A request that changes
nothing writes no event. The key cannot be changed. `200` with the project,
`404`, `422`. The `project.updated` event carries the new name, and for the
rules only `{"rules": {"changed": true, "chars": n}}`, never the text.

`repo_path` and `base_branch` are an exception to "anyone who may write": only
the **owner** may set them (`403` for a member or an agent's key), because they
decide which folder of this machine runs work in.

- `repo_path` is an absolute folder (at most 1000 characters). The server runs
  `git` on it before saving: the folder must exist and be inside a non-bare git
  repository with at least one commit. What is stored is the repository's
  top-level folder, so a subfolder is accepted and normalised. `null` or blank
  clears it, and runs go back to getting an empty folder of their own.
- `base_branch` (at most 200 characters) is the branch new working copies start
  from. It must exist in the repository (a local branch, or `origin/<name>`), and
  a repository must be set first. `null` or blank means "the repository's
  default": `origin/HEAD`, else the branch that is checked out, else `main`, else
  `master`.
- `422` says what is wrong in words, for example "git was not found on PATH" when
  `git` is not installed.
- The event carries only `{"repo_path": {"changed": true, "set": true|false}}`
  (and the same for `base_branch`), never the folder or the branch name.

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
and the user is no longer restricted (see Authentication). The agent's prompt
template overrides go with it. `204`.

### `POST /api/v1/agents/{id}/enable`

Owner only. `200` with the agent; repeating it changes nothing.

### `POST /api/v1/agents/{id}/disable`

Owner only. `200` with the agent. The agent's keys then get `403` "agent is
disabled" on everything except `GET /api/v1/auth/me`, and no run is started
for it. Repeating it changes nothing.

## Prompt templates

The first message of a run is rendered from a template, so an agent starts
with the issue, why it was started, the recent comments, the open blockers and
the project's rules, and a person can change what it says. The pieces that do
not need a run exist now: the template language, storage, resolution, the
preview and the variable catalogue. The runner (see Runs) calls one function,
`aurora_store::prompt::render_prompt`, and stores what it returns.

**Which template applies**, most specific first:

1. the agent's override for the issue's project and issue type,
2. the agent's override for the issue's project,
3. the agent's own `instructions`, unless blank,
4. the built-in default (`default_template` of `GET /prompt/variables`).

An override always names a project, because an issue type belongs to a project;
the agent-wide default is the `instructions` field of the agent. There is one
override per (agent, project) and one per (agent, project, issue type).
Overrides and instructions are owner-only text: members and agent keys cannot
read or change them (`403`), and events carry only their length, never the
text. Deleting an issue type that an override points at is refused with `409`.

**The language.** Plain text with tags in double braces:

| Tag | Meaning |
|---|---|
| `{{name}}` | Inserts a variable (see the table below). Numbers and booleans are written as `12`, `true`. |
| `{{#if name}}...{{else}}...{{/if}}` | Renders the first part when the value is true: a non-empty text or list, `true`, or a number other than 0. `{{else}}` is optional. |
| `{{#each list}}...{{/each}}` | Repeats the body for each item. Inside, `{{this}}` is the item of a text list, `{{this.field}}` a field of a record item, `{{@index}}` the position from 0. `this` is always the innermost loop. |
| `{{!-- note --}}` | A comment; dropped. |
| `\{{` | A literal `{{`. |

A block tag or comment alone on its line removes the whole line, so templates
can be laid out readably. There are no expressions, comparisons or includes,
and nothing is executed. The output is plain text and is not escaped.
**A value is inserted as it is and is never scanned again**: an issue
description that contains `{{issue.title}}` stays that text, so nobody can
inject tags through an issue or a comment.

**Validation** is strict and names the problem with its line and column in the
`422` message, for example "unknown variable `issue.titel`; see the list of
available variables (line 2, column 4)". It catches unknown variables or
fields, a list printed as text, `this` or `@index` outside a loop, an unclosed
or unmatched block, more than one `{{else}}`, and tags that are not tags.
Instructions are validated the same way when an agent is created or updated,
and a saved template is always valid (the `prompt_templates.valid` check of
`aurora check` proves it).

**Limits.** A template has at most 20000 characters and blocks nest at most 4
deep. The rendered message has at most 200000 characters. The context is
bounded too: the last 30 comments that are not deleted (each cut at 4000
characters, with `comments.omitted` counting the older ones) and at most 100
items in any other list. When the message would be longer than the limit it is
not refused but shortened, largest first: the longest text among all values is
repeatedly halved (never below 200 characters) and ends with `… [truncated]`
until the message fits; only a template that is too long by itself (a loop
over a loop) has its output cut at the limit with the same marker. `truncated`
in the result says that anything was cut or left out, including comments and
list items beyond the bounds.

**Variables.** Names are flat, with dots (`issue.title`, `trigger.comment`).
`GET /prompt/variables` serves this table from the code that validates
templates, and a test checks that every name is listed here.

| Variable | Type | Meaning |
|---|---|---|
| `issue.key` | text | The issue key, e.g. `TRK-12`. |
| `issue.number` | number | The issue number within its project. |
| `issue.title` | text | The issue title. |
| `issue.description` | text | The description (may be empty). |
| `issue.type` | text | The issue type's name. |
| `issue.column` | text | The name of the issue's column. |
| `issue.state_type` | text | `backlog`, `unstarted`, `started`, `completed` or `canceled`. |
| `issue.priority` | text | `urgent`, `high`, `medium`, `low`; empty when none. |
| `issue.assignee` | text | The assignee's name; empty when unassigned. |
| `issue.labels` | list of text | The label names (`{{this}}`). |
| `issue.parent` | text | The parent's key; empty for a top-level issue. |
| `issue.children` | list | Sub-issues not archived; items have `key`, `title`, `state_type`. |
| `issue.url` | text | The issue's page: absolute with a public URL, otherwise `/p/KEY/N`. |
| `project.key` | text | The project key. |
| `project.name` | text | The project name. |
| `project.rules` | text | The project's rules text (may be empty). |
| `initiative.name` | text | The issue's initiative; empty when none. |
| `comments` | list | The last 30 comments not deleted, oldest first; items have `author`, `kind` (`human` or `agent`), `created` (ISO-8601 UTC), `body`, `is_trigger`. |
| `comments.omitted` | number | How many older comments are not in `comments`. |
| `links.blocks` | list | Issues this issue blocks; items as `issue.children`. |
| `links.blocked_by` | list | Issues that block this one, finished or not. |
| `links.related` | list | Related issues. |
| `blockers` | list | Only the blockers still open (not completed, canceled or archived). |
| `documents` | list | Documents linked to the issue, not archived; items have `title`, `id`. |
| `trigger` | text | Why the run started: `assigned`, `mentioned`, `schedule` or `manual`. |
| `trigger.assigned` | boolean | True for an assignment. |
| `trigger.mentioned` | boolean | True for a mention. |
| `trigger.schedule` | boolean | True for a schedule. |
| `trigger.manual` | boolean | True when a person started the run. |
| `trigger.comment` | text | The comment that mentioned the agent; empty otherwise. |
| `trigger.actor` | text | Who caused the run; empty when unknown. |
| `agent.name` | text | The agent's name. |
| `workdir` | text | The run's working copy (its git worktree folder, see Runs); empty when the run has none of its own. |
| `run.id` | text | The run's id; empty in a preview. |
| `run.branch` | text | The git branch of the run's working copy; empty when it has none. |
| `server.url` | text | The MCP endpoint: absolute with a public URL, otherwise `/mcp`. |
| `now` | text | The current time, ISO-8601 UTC. |

Links are absolute only when the server is started with `--public-url <url>`
(or `AURORA_PUBLIC_URL`), an `http` or `https` address without user info, query
or fragment; a trailing slash is dropped. Otherwise they stay relative.

**What the runner supplies (TRK-15, TRK-17).** `render_prompt(conn, now,
&PromptRequest)` takes the agent, the issue (`project` key and `number`), the
trigger (`TriggerCtx`: kind, optional mentioning comment, optional actor),
`workdir`, `run_id`, `public_url` and, when a person edited the message by
hand, `override_template`. It is read-only, runs a fixed number of queries and
returns `{text, chars, template_source, template_id, truncated,
variables_used}`; the run stores exactly that text and `template_source` /
`template_id`, so a later edit of the template never changes what a past run
was told. It fails with `404` (unknown agent, project or issue) or `422` (the
comment is not on the issue, or the stored template no longer validates).
No run table exists yet: storing the text, showing it on the run and a
"start with an edited message" input belong to those tasks.

### `GET /api/v1/agents/{id}/templates`

Owner only (`403`). `{"default_source": "builtin" | "agent", "templates": [...]}`
where `default_source` says what applies when no override matches, and each
template is `{id, agent_id, project, issue_type_id, issue_type, body, created,
updated}` ordered by project key, the project-wide override before the
per-type ones. `404` for an unknown agent.

### `PUT /api/v1/agents/{id}/templates`

Owner only. Body `{"project": "KEY", "issue_type_id"?: n, "body": "..."}`
creates the override for that scope (`201`) or replaces its text (`200`), with
the template as the body. Saving the text that is already stored writes
nothing. Takes a request id (see Idempotency). `403`, `404` for an unknown
agent, `422` for an unknown project or issue type, an issue type of another
project, an empty text, more than 20000 characters or a template that does not
validate (the message has the line and column). One `prompt_template.saved`
event with `{agent_id, project, issue_type, created, body: {changed, chars}}`.

### `DELETE /api/v1/agents/{id}/templates/{template_id}`

Owner only. `204`; `404` when the template does not exist or is another
agent's. One `prompt_template.deleted` event. Takes a request id.

### `POST /api/v1/agents/{id}/prompt-preview`

The exact message a run of this agent would get for an issue, without
starting anything (use `POST /agents/{id}/runs` to start). Owners and members may ask; an agent's key gets `403`.
Body `{"project": "KEY", "number": n, "trigger"?: "assigned" | "mentioned" |
"schedule" | "manual" (default "manual"), "comment_id"?: n, "template"?:
"..."}`; `template` renders that text instead of the stored one (an unsaved
edit). `200` with `{text, chars, template_source, template_id, truncated,
variables_used}` where `template_source` is `builtin`, `agent`, `project`,
`project_type` or `inline`. `404` for an unknown agent, project or issue,
`422` for a template that does not validate (with line and column), an unknown
trigger, or a `comment_id` that is not a comment of the issue. It changes
nothing, writes no event and ignores a request id. `workdir` and `run.id` are
empty in a preview. It is a `POST`, so a key with read scope cannot use it.

### `GET /api/v1/prompt/variables`

Any signed-in user. `{"variables": [...], "limits": {...},
"default_template": "..."}`: each variable is `{name, type, description,
example}` with `type` one of `text`, `number`, `boolean`, `list`; a list also
has `item` (`text` or `record`) and, for records, `fields: [{name, type,
description}]`. `limits` has `max_template_chars`, `max_depth`,
`max_output_chars`, `max_comments` and `max_comment_chars`.

## Runs

A run is one launch of an agent's command for one issue. The server starts it
when an issue is assigned to the agent's user or the agent is mentioned in a
comment (when `on_assign` / `on_mention` allow it and the agent is enabled and
may use the project), or when the owner or a member starts one by hand. The
command runs as a plain child process: the first message goes to its standard
input (or into a `{prompt}` argument), `{issue}`, `{run_id}` and `{mcp_config}`
are substituted in the arguments, and the environment carries
`AURORA_URL`, `AURORA_MCP_URL`, `AURORA_API_KEY` (a write key made for this run
only), `AURORA_RUN_ID`, `AURORA_ISSUE`, `AURORA_PROJECT` and `AURORA_TRIGGER`, plus
`AURORA_WORKDIR` (the folder the process starts in) and, for a run with a
working copy of its own, `AURORA_BRANCH`.
The key is revoked in the transaction that records the end of the run.
Standard output and error are read as they arrive and kept as the run's
**transcript** (see below); the last 64 KiB are also kept on the run itself as
`output_tail`; the exit status is the result. There is at most one active run
(`queued`, `starting` or `running`) per issue; a trigger that arrives meanwhile
is dropped.

**Limits and the queue.** At most `AURORA_MAX_RUNS` (`--max-runs`, 1 to 64,
default 4) runs are `starting` or `running` at the same time, all agents
together, and at most the agent's `max_concurrent` (1 to 16, default 1) per
agent. A run that would pass either limit is created `queued` instead (the
decision is made in the same transaction as the creation, so two triggers can
never both pass a limit). A queued run is only an intent: it records who or what
asked for it (the reason, the event, the person, the comment, the manual
`message` and `base_branch`) and has **no process, key, working copy or
transcript**; its first message is written when it starts, so it sees the issue
as it is then. The scheduler promotes queued runs oldest first (`queued_at`,
then id) whenever a run ends or a limit changes, at the latest 5 seconds later;
an agent that is at its own limit is skipped and does not hold other agents'
runs back. Before a queued run starts it is checked again: when its agent was
disabled (or its user), lost access to the project, or the issue was archived,
the run ends `canceled` with the reason in `error` (`agent disabled`, `agent
user disabled`, `agent no longer has access to project KEY`, `issue archived`)
instead of starting. If `AURORA_MAX_QUEUE_WAIT_SECS` is set above 0, a queued
run that has waited that long ends `canceled` with `queue wait exceeded`; the
default is no limit, and the time a run waits never counts against its
timeout, which starts when the process does. An agent may have at most
`AURORA_MAX_QUEUED_PER_AGENT` queued runs (default 20, 0 to 1000); a trigger
beyond that is dropped and counted (`queue_full_drops` in `GET /queue`, and a
warning in the log), and a manual start answers `409` with `queue full`. Queued
runs survive a restart and are promoted as the limits allow, whereas a run that
was `starting` or `running` when the server died is marked `interrupted` at the
next start. A canceled run frees its slot as soon as the cancel commits, while
its process tree may take a few seconds to go.

**A working copy per run.** When the project has a `repo_path` and the agent has
no `work_dir` of its own, every run works in its own git worktree, so two
writing agents never touch the same files. The worktree is made from the base
branch (the project's `base_branch`, or the `base_branch` given when a person
starts the run) at `<data>/worktrees/<run id>` on a new branch
`aurora/<KEY>-<n>-run<id>` (letters, digits, `-` and `_` only; at most 100
characters). The process starts in it, and the prompt's `workdir` and
`run.branch` name it. An agent with an explicit `work_dir` gets no isolation
(`worktree_state` `none`), and neither does a project without a repository: both
behave exactly as before. If the worktree cannot be made, the run fails with
`could not create worktree: <reason>` and nothing is left behind.

When the run ends, however it ends (success, failure, timeout, cancel, shutdown),
the worktree is inspected: it **has changes** when `git status` lists any file
(untracked ones included) or its `HEAD` is not the commit it started from. A
worktree without changes is removed together with its branch (`removed`). One
with changes is never deleted: it stays, with its branch, for a person to look at
(`kept`). When the server cannot tell (git failed, a crash), it is kept too, with
a `worktree_note`. Cleanup problems never fail a run.

**The transcript.** Everything the process printed is appended, as it arrives, to
an append-only file `<data>/transcripts/<run id>.log` (not to the database: it is
large and changes all the time), standard output and standard error merged in the
order they were read, each piece tagged with its stream. The file holds one JSON
object per line, `{"t": ms since the process started, "s": "o" | "e" | "x", "d":
"text"}`: `o` is standard output, `e` standard error, `x` a note written by
Aurora itself and not output of the process (`run canceled`, `run timed out`,
`server stopped`, `process exited with code 3`, `transcript truncated at N
bytes`). A line of output is one frame including its line ending; a longer line
is cut into frames of at most 8 KiB. Bytes that are not valid UTF-8 are replaced
by U+FFFD. Batches are appended at least every 250 ms or 8 KiB, so a crash loses
at most the last batch; the file is synced once when the run ends. It is capped
(default 8 MiB, `AURORA_TRANSCRIPT_MAX_BYTES` or `--transcript-max-bytes`, 1024 to
1 GiB): at the cap one `x` frame says so and nothing more is written, while the
process keeps running and its output tail is still recorded. Before it is written
the text has the run's own API key replaced by `[key]` and the project's
repository path (either slash spelling) by `the repository`; this is exact-match,
best effort (a secret split across a flush of a line that stays unterminated for
more than 250 ms, or across a 64 KiB line, is not found). Every end the server is
alive to see (cancel, timeout, shutdown, a failing exit) ends the file with an
`x` frame; a successful exit has none, and a crash leaves no marker (the run is
`interrupted` at the next start, and the file is left as it is: readable up to
its last complete line). Transcripts are not deleted by Aurora (retention is a
later task) and belong in backups of the data folder. They are not exposed
through MCP: an agent could read another run's output.

A run is `{id, agent_id, agent, project, issue, issue_number, reason, status,
trigger_event_id, pid, template_source, template_id, edited, exit_code, error,
output_truncated, started, finished, created, created_by, worktree_state,
worktree_path, branch, base_ref, base_commit, has_changes, changed_files, ahead,
worktree_note, transcript_bytes, transcript_truncated, queued_at,
queue_position, reaped}`. `reaped` (TRK-45) is what start-up found of the process
of a run it marked `interrupted` (`killed leftover process <pid>`, `process
already gone`, `pid reused, left alone`, `pid unknown`); null for every other
run, and hidden from an agent's key. When start-up interrupts a run it also
comments on the issue (as the run's agent, prefixed `[Aurora]`, no paths) and
moves a card that is still in a `started` column to the first `unstarted` one.
`queued_at` is when the run was queued (kept after it
started; null for a run that started at once) and `queue_position` its 1-based
place among all queued runs, oldest first (null unless the run is `queued`;
computed when read, so it moves up as runs ahead of it start or are canceled).
`transcript_bytes` is
the size of the transcript file when the run ended (0 until then, and after a
crash); `transcript_truncated` says the cap was reached. `worktree_state` is `none`, `created` (made, not yet settled),
`removed` or `kept`; `base_ref` is the branch it started from and `base_commit`
its commit at that moment; `changed_files` counts files `git status` lists and
`ahead` the commits on the run's branch beyond `base_commit`. A registered
agent's key sees `branch` and the state but not `worktree_path` or
`worktree_note`. `reason` is
`assigned`, `mentioned`, `manual` or `schedule`; `status` is `queued`,
`starting`, `running`, `succeeded`, `failed`, `interrupted` or `canceled`.
**`interrupted` is what a crash or a stop of the server leaves behind** for a run
that was active (`starting` or `running`; error `server restarted` or `server
stopped`); it is a distinct final status from `failed`, which means the process
ran (or could not start) and went wrong, and it is the outcome a crash has: no
active run survives a restart. Times are
milliseconds since the epoch. The detail of a run adds `message` (the exact
first message) and `output_tail`; both can hold secrets, so only the owner and
members get them. A registered agent's key sees the runs of its allowed
projects without `message` and `output_tail`.

Events: `run.created`, `run.promoted`, `run.started` and `run.finished` carry no
message and no output; there is no event per chunk of output. `run.created` also
has `branch`, `worktree_state` and `queued` (true when the run waits; its
`branch` is then null), `run.promoted` (`queued` to `starting`, written in the
transaction that mints the key and plans the working copy) has `branch` and
`worktree_state`, and `run.finished` has `worktree_state`,
`has_changes` and, when the runner finished the file itself, `transcript_bytes`
(absent for a cancel by a person: it is committed before the process is stopped,
and the size is recorded on the run a moment later); no folder is ever in an
event. A cancel finishes the run before its process is stopped; the worktree and
the transcript size are settled moments later, without another event, and the
run then says `removed` or `kept`. The cancel request waits for that (see
below), so the run it returns is normally the settled one.

### `POST /api/v1/agents/{id}/runs`

Owner or member (an agent's key gets `403`). Body `{"issue": "KEY-12",
"message"?: "...", "trigger_comment_id"?: n, "base_branch"?: "name"}`;
`message` replaces the rendered first message and is sent verbatim.
`base_branch` starts this run's worktree from another branch of the project's
repository; `422` when it does not exist, or when the run gets no working copy
(no repository, or the agent has a `work_dir`). `201` with the run detail; when a
limit is reached the run in it is `queued` (still `201`: read `status`), and
`queue_position` says where. `404` for an unknown agent, `422` for a malformed or
unknown issue, a disabled agent or user, or a project the agent may not use, `409`
when the issue already has an active (queued included) run or the agent's queue is
full, `503` when this server has no runner. It takes a request id: a
repeat returns the recorded response and starts no second process.

### `GET /api/v1/runs`

Query: `issue` (`KEY-12`), `agent` (id), `status` (`queued`, `starting`,
`running`, `succeeded`, `failed`, `interrupted` or `canceled`), `has_changes` (`true` lists
runs whose working copy has changes, `false` the others), `limit` (default 50,
max 200), `offset`. Newest first, as an array of runs (no message or output). Only
runs of projects the key may use are listed; asking for another project's issue
is `403`. `422` for a malformed `issue`, an unknown `status` or a `has_changes`
that is not `true` or `false`.

### `GET /api/v1/runs/{id}`

The run: the detail for owners and members, the plain run for an agent's key.
`404`, or `403` for a project the key may not use.

### `POST /api/v1/runs/{id}/cancel`

Owner or member (an agent's key gets `403`). The run becomes `canceled` and its
key is revoked in this request's transaction, which also records the request id.
A `queued` run has no process: it is `canceled` at once (event `run.finished`
with status `canceled`) and the request answers without waiting, with the run
(no key to revoke, no transcript).
The process tree is stopped right after the commit (`taskkill /T /F` on Windows,
the process group elsewhere), the transcript gets its `run canceled` frame and
the working copy is settled. The request waits for that, at most 5 seconds, and
answers `200` with the run as it is then (the detail, as `GET /runs/{id}` gives
an owner; `worktree_state` is normally already `removed` or `kept`). If the
runner does not settle in time the answer is the run as committed (its
`worktree_state` may still say `created`, and `transcript_bytes` 0); a later
`GET` shows the settled run. `409` when the run already finished, `404`, `503`
without a runner. It takes a request id; a repeat returns the response recorded
with the change (the run as committed, with the `Idempotent-Replay` header) and
stops nothing a second time.

### `GET /api/v1/runs/{id}/transcript`

Owner or member only: the output can hold secrets, so a registered agent's key
gets `403` even for a run of its own project (the same rule as `output_tail`).
Query `after` (a byte offset into the transcript file, default 0) and `limit`
(about how many bytes of lines to return: default 65536, at most 262144; a line
longer than that is still returned whole). `200` with `{"entries": [{"t", "s",
"d"}, ...], "next": n, "done": bool, "truncated": bool, "status": "running"}`.
Only whole lines are returned, so a reader that overlaps the writer never gets a
torn line, and `next` is the offset to pass as `after` the next time: poll with
`after=next` (every second while `done` is false). A line that a crash left half
written at the end of the file is never returned and is not an error; a complete
line that is not a valid frame is skipped. `done` is true only when the run has
finished, the runner has finished writing the file (after a cancel that is a
moment after the run says `canceled`) and this answer reached the end of the
file. `truncated` is true once the size cap was hit. A running run that has not
printed anything yet answers `200` with no entries; `404` for an unknown run, or
a finished run that never had a transcript (it failed before its process
started). The answer is `no-store`.

### `GET /api/v1/runs/{id}/transcript.txt`

Same access. The whole transcript as `text/plain; charset=utf-8`: the output of
both streams concatenated in order (stderr is not marked), with each note of
Aurora's own as a line `[aurora: run canceled]`. With `?download=1` the answer
also has `Content-Disposition: attachment; filename="run-<id>.txt"`. `404` as
above (a running run without output gives an empty text). The text is built in
memory, so it is bounded by the transcript's size cap.

### `GET /api/v1/queue`

Read scope. The run queue and the limits: `{"running": n, "limit": n, "queued":
[{"run_id", "agent_id", "agent", "project", "issue", "queued_at", "position",
"waiting_on"}], "queue_full_drops": n}`. `running` is the number of runs
`starting` or `running`, `limit` the overall limit (`AURORA_MAX_RUNS`). `queued`
is oldest first; `position` is the 1-based place among all queued runs and
`waiting_on` is `agent` when the run's agent is at its own `max_concurrent`,
else `global` (the overall limit, or an older run that goes first).
`queue_full_drops` counts triggers dropped since the server started because
their agent's queue was full (it restarts at 0). Query `agent` (id) narrows
`queued` and `running` to one agent (`404` for an unknown agent). A registered
agent's key sees only the entries of its allowed projects, and `running` counts
only runs of those projects; `limit` and `queue_full_drops` are server-wide.
`401` without a key, `503` when this server has no runner.

### `GET /api/v1/admin/housekeeping`

Owner only (`403` for members and agents, `401` without a key). What start-up
reconciliation leaves for a person (TRK-45), computed when asked and read-only:
`{"leftover_processes": [{"run", "pid", "note"}], "unknown_working_copies":
[{"name", "kind"}], "kept_working_copies": [{"run", "branch", "state",
"folder_exists"}], "checked_at"}`. `leftover_processes` lists the runs with a
`reaped` note (newest first, at most 500): `killed leftover process <pid>`,
`process already gone`, `pid reused, left alone`, `pid unknown`, `could not kill
leftover process <pid>`. `unknown_working_copies` are entries of
`<data folder>/worktrees/` that no run with a `created` or `kept` working copy
accounts for; `name` is relative to the data folder (`worktrees/<name>`), `kind`
`folder` or `file`. `kept_working_copies` are the runs whose working copy is
`created` or `kept`, with whether `worktrees/<run id>` exists. Nothing is ever
deleted, killed or restarted by this endpoint or by start-up; the same lists are
shown in the "Housekeeping" panel of the Agents page (owner only).

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
