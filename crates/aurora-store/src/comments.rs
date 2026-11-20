//! Comments on issues: one level of replies, soft delete and `@mentions`.
//!
//! Plain functions over a transaction (writes) or a connection (reads), like
//! [`crate::links`]. Every change appends its events in the same transaction.
//! Issues are addressed by project key and number, comments by id (which must
//! belong to the project).
//!
//! * A body is plain text of 1-[`MAX_BODY_CHARS`] characters after trimming.
//!   It is never interpreted as HTML; the UI escapes it.
//! * A reply's parent must be a live top-level comment of the same issue.
//! * Who may change a comment: its author and the owner. Everyone who may
//!   write may add one. Version 1 has no per-project permissions.
//! * Deleting keeps the row (so replies stay attached) and sets `deleted`;
//!   reads blank the body. The text stays in the database file, never in an
//!   event. A deleted comment that nothing visible hangs off is left out of
//!   [`list_comments`]; a deleted reply is always left out.
//!
//! # Mentions
//!
//! `@name` in a body mentions the user with that name (compared ASCII
//! case-insensitively, like the unique index on user names) when the user
//! exists and is not disabled; humans and agents alike. The name is the
//! longest run of letters, digits, `.`, `_` and `-` after the `@` that is a
//! user name and is not followed by a letter, digit or `_` (so `@robot.`
//! and `@robot-x` mention `robot`, `@robotx` does not). The `@` must follow the start of
//! the text, whitespace or punctuation: `a@b.com` mentions nobody. Names
//! containing other characters (spaces) cannot be mentioned. Unknown names
//! stay plain text. Creating a comment emits one `mention.created` per
//! distinct user (the author included: nothing is special-cased); an edit
//! emits it only for users added by that edit.
//!
//! # Events
//!
//! All have entity kind `issue` and the issue's id as entity id, so they show
//! in the issue's own log. The full body is in none of them.
//!
//! * `comment.created` `{comment_id, parent, author: {id, name, kind},
//!   mentions: [name]}`
//! * `comment.edited` `{comment_id, mentions: [name]}` (the mentions now in
//!   the body)
//! * `comment.deleted` `{comment_id}`
//! * `mention.created` `{comment_id, issue: {number, identifier}, user_id,
//!   user_name, user_kind, author_id, snippet}`, `snippet` being the first
//!   [`SNIPPET_CHARS`] characters of the body. Agent runners consume these
//!   filtered by `user_kind == "agent"`; delivery is at least once.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::StoreError;
use crate::auth::Principal;
use crate::links::IssueRef;
use crate::projects::{Project, Rejection, emit, get_project, invalid, not_found};

pub const MAX_BODY_CHARS: usize = 20_000;
pub const SNIPPET_CHARS: usize = 200;
/// User names are at most this long (see `auth::clean_name`), so no longer
/// candidate is tried.
const MAX_MENTION_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommentAuthor {
    pub id: i64,
    pub name: String,
    /// `human` or `agent`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MentionedUser {
    pub user_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comment {
    pub id: i64,
    pub issue: IssueRef,
    /// The top-level comment this one replies to.
    pub parent: Option<i64>,
    pub author: CommentAuthor,
    /// Empty once deleted.
    pub body: String,
    pub created: i64,
    pub edited: Option<i64>,
    pub deleted: Option<i64>,
    pub mentions: Vec<MentionedUser>,
    /// Only on top-level comments of [`list_comments`]; empty elsewhere.
    pub replies: Vec<Comment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewComment {
    pub body: String,
    #[serde(default)]
    pub parent: Option<i64>,
}

// ---- mention parsing ----

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// What may stand right before an `@` that starts a mention: not a character
/// an email address or a name is made of.
fn may_precede_mention(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'))
}

/// Finds the mentions in `body` in order of appearance, duplicates included.
/// `resolve` is asked about candidate names, longest first, and says whether
/// one is a mention; each result has the byte range of `@name` in `body`.
pub fn scan_mentions<T>(
    body: &str,
    mut resolve: impl FnMut(&str) -> Option<T>,
) -> Vec<(Range<usize>, T)> {
    let mut found = Vec::new();
    let mut skip_to = 0;
    let mut before: Option<char> = None;
    for (at, c) in body.char_indices() {
        let previous = before.replace(c);
        let allowed = match previous {
            None => true,
            Some(p) => may_precede_mention(p),
        };
        if c != '@' || at < skip_to || !allowed {
            continue;
        }
        let start = at + 1;
        let mut ends = Vec::new();
        for (offset, ch) in body[start..].char_indices().take(MAX_MENTION_CHARS) {
            if !is_name_char(ch) {
                break;
            }
            ends.push(start + offset + ch.len_utf8());
        }
        for &end in ends.iter().rev() {
            // `@alicex` is not a mention of `alice`: a name ends at a word boundary.
            let continues = body[end..]
                .chars()
                .next()
                .is_some_and(|next| next.is_alphanumeric() || next == '_');
            if continues {
                continue;
            }
            if let Some(user) = resolve(&body[start..end]) {
                found.push((at..end, user));
                skip_to = end;
                break;
            }
        }
    }
    found
}

/// The byte ranges of `@name` in `body` for the given mentioned names, for
/// rendering. Uses the same rules as writing a comment did.
pub fn mention_ranges(body: &str, names: &[&str]) -> Vec<Range<usize>> {
    scan_mentions(body, |candidate| {
        names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(candidate))
            .then_some(())
    })
    .into_iter()
    .map(|(range, ())| range)
    .collect()
}

#[derive(Debug, Clone)]
struct Mentionable {
    id: i64,
    name: String,
    kind: String,
}

/// Existing, enabled users by ASCII-lowercased name.
fn mentionable_users(conn: &Connection) -> Result<HashMap<String, Mentionable>, StoreError> {
    let mut stmt = conn.prepare("SELECT id, name, kind FROM user WHERE disabled = 0")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Mentionable {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|user| (user.name.to_ascii_lowercase(), user))
        .collect())
}

/// The distinct users `body` mentions, in order of first appearance.
fn resolve_mentions(conn: &Connection, body: &str) -> Result<Vec<Mentionable>, StoreError> {
    if !body.contains('@') {
        return Ok(Vec::new());
    }
    let users = mentionable_users(conn)?;
    let mut seen = HashSet::new();
    Ok(scan_mentions(body, |candidate| {
        users.get(&candidate.to_ascii_lowercase()).cloned()
    })
    .into_iter()
    .map(|(_, user)| user)
    .filter(|user| seen.insert(user.id))
    .collect())
}

// ---- reads ----

const COMMENT_SELECT: &str = "SELECT c.id, i.number, c.parent_id, c.author_id, u.name, u.kind,
            c.body, c.created, c.edited, c.deleted
     FROM comment c
     JOIN issue i ON i.id = c.issue_id
     JOIN user u ON u.id = c.author_id";

fn comment_row(row: &Row<'_>, key: &str) -> rusqlite::Result<Comment> {
    let number: i64 = row.get(1)?;
    let deleted: Option<i64> = row.get(9)?;
    let body: String = row.get(6)?;
    Ok(Comment {
        id: row.get(0)?,
        issue: IssueRef {
            number,
            identifier: format!("{key}-{number}"),
        },
        parent: row.get(2)?,
        author: CommentAuthor {
            id: row.get(3)?,
            name: row.get(4)?,
            kind: row.get(5)?,
        },
        body: if deleted.is_some() {
            String::new()
        } else {
            body
        },
        created: row.get(7)?,
        edited: row.get(8)?,
        deleted,
        mentions: Vec::new(),
        replies: Vec::new(),
    })
}

fn mentions_of(
    conn: &Connection,
    sql: &str,
    arg: i64,
) -> Result<HashMap<i64, Vec<MentionedUser>>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([arg], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                MentionedUser {
                    user_id: row.get(1)?,
                    name: row.get(2)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut found: HashMap<i64, Vec<MentionedUser>> = HashMap::new();
    for (comment, user) in rows {
        found.entry(comment).or_default().push(user);
    }
    Ok(found)
}

/// One comment of the project, with its mentions (none once deleted).
pub fn get_comment(
    conn: &Connection,
    project_key: &str,
    comment_id: i64,
) -> Result<Comment, StoreError> {
    let project = get_project(conn, project_key)?;
    load_comment(conn, &project, comment_id)
}

fn load_comment(
    conn: &Connection,
    project: &Project,
    comment_id: i64,
) -> Result<Comment, StoreError> {
    let mut comment = conn
        .query_row(
            &format!("{COMMENT_SELECT} WHERE c.id = ?1 AND i.project_id = ?2"),
            params![comment_id, project.id],
            |row| comment_row(row, &project.key),
        )
        .optional()?
        .ok_or_else(|| not_found("comment"))?;
    if comment.deleted.is_none() {
        comment.mentions = mentions_of(
            conn,
            "SELECT m.comment_id, u.id, u.name FROM comment_mention m
             JOIN user u ON u.id = m.user_id
             WHERE m.comment_id = ?1 ORDER BY m.position",
            comment_id,
        )?
        .remove(&comment_id)
        .unwrap_or_default();
    }
    Ok(comment)
}

/// The comments of an issue: top-level ones in creation order, each with its
/// `replies` in creation order. See the module docs for what is left out.
pub fn list_comments(
    conn: &Connection,
    project_key: &str,
    issue_number: i64,
) -> Result<Vec<Comment>, StoreError> {
    let project = get_project(conn, project_key)?;
    let issue_id = issue_id(conn, &project, issue_number)?;
    let mut stmt = conn.prepare(&format!(
        "{COMMENT_SELECT} WHERE c.issue_id = ?1 ORDER BY c.id"
    ))?;
    let all = stmt
        .query_map([issue_id], |row| comment_row(row, &project.key))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut mentions = mentions_of(
        conn,
        "SELECT m.comment_id, u.id, u.name FROM comment_mention m
         JOIN comment c ON c.id = m.comment_id
         JOIN user u ON u.id = m.user_id
         WHERE c.issue_id = ?1 ORDER BY m.comment_id, m.position",
        issue_id,
    )?;

    let mut top: Vec<Comment> = Vec::new();
    let mut replies: HashMap<i64, Vec<Comment>> = HashMap::new();
    for mut comment in all {
        if comment.deleted.is_none() {
            comment.mentions = mentions.remove(&comment.id).unwrap_or_default();
        }
        match comment.parent {
            Some(parent) if comment.deleted.is_none() => {
                replies.entry(parent).or_default().push(comment);
            }
            Some(_) => {}
            None => top.push(comment),
        }
    }
    top.retain_mut(|comment| {
        comment.replies = replies.remove(&comment.id).unwrap_or_default();
        comment.deleted.is_none() || !comment.replies.is_empty()
    });
    Ok(top)
}

fn issue_id(conn: &Connection, project: &Project, number: i64) -> Result<i64, StoreError> {
    conn.query_row(
        "SELECT id FROM issue WHERE project_id = ?1 AND number = ?2",
        [project.id, number],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| not_found("issue"))
}

// ---- writes ----

fn clean_body(body: &str) -> Result<String, StoreError> {
    let body = body.trim();
    if (1..=MAX_BODY_CHARS).contains(&body.chars().count()) {
        Ok(body.to_string())
    } else {
        Err(invalid(format!(
            "comment must be 1-{MAX_BODY_CHARS} characters"
        )))
    }
}

fn snippet(body: &str) -> String {
    body.chars().take(SNIPPET_CHARS).collect()
}

fn forbidden() -> StoreError {
    Rejection::Forbidden("only the author or the owner can change a comment".to_string()).into()
}

#[allow(clippy::too_many_arguments)]
fn emit_mention(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project: &Project,
    issue_id: i64,
    comment: &Comment,
    user: &Mentionable,
    body: &str,
) -> Result<(), StoreError> {
    let issue = &comment.issue;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        issue_id,
        "mention.created",
        &json!({
            "comment_id": comment.id,
            "issue": { "number": issue.number, "identifier": issue.identifier },
            "user_id": user.id,
            "user_name": user.name,
            "user_kind": user.kind,
            "author_id": comment.author.id,
            "snippet": snippet(body),
        }),
    )
}

/// Adds a comment to an issue, or a reply when `new.parent` is set: a live
/// top-level comment of the same issue (invalid otherwise).
pub fn add_comment(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    issue_number: i64,
    new: NewComment,
) -> Result<Comment, StoreError> {
    let body = clean_body(&new.body)?;
    let project = get_project(tx, project_key)?;
    let issue_id = issue_id(tx, &project, issue_number)?;
    if let Some(parent) = new.parent {
        let found: Option<(i64, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT issue_id, parent_id, deleted FROM comment WHERE id = ?1",
                [parent],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match found {
            Some((issue, None, None)) if issue == issue_id => {}
            Some((issue, _, _)) if issue != issue_id => {
                return Err(invalid("the comment to reply to is not on this issue"));
            }
            Some((_, Some(_), _)) => {
                return Err(invalid(
                    "replies nest one level: reply to the top-level comment",
                ));
            }
            Some(_) => return Err(invalid("the comment to reply to was deleted")),
            None => return Err(invalid(format!("comment {parent} not found"))),
        }
    }
    let mentions = resolve_mentions(tx, &body)?;
    tx.execute(
        "INSERT INTO comment (issue_id, parent_id, author_id, body, created)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![issue_id, new.parent, actor.user_id, body, now],
    )?;
    let id = tx.last_insert_rowid();
    store_mentions(tx, id, &mentions)?;
    let comment = load_comment(tx, &project, id)?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        issue_id,
        "comment.created",
        &json!({
            "comment_id": id,
            "parent": new.parent,
            "author": {
                "id": comment.author.id,
                "name": comment.author.name,
                "kind": comment.author.kind,
            },
            "mentions": mentions.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        }),
    )?;
    for user in &mentions {
        emit_mention(tx, now, actor, &project, issue_id, &comment, user, &body)?;
    }
    Ok(comment)
}

fn store_mentions(
    tx: &Transaction<'_>,
    comment_id: i64,
    mentions: &[Mentionable],
) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM comment_mention WHERE comment_id = ?1",
        [comment_id],
    )?;
    for (position, user) in (0_i64..).zip(mentions) {
        tx.execute(
            "INSERT INTO comment_mention (comment_id, user_id, position) VALUES (?1, ?2, ?3)",
            params![comment_id, user.id, position],
        )?;
    }
    Ok(())
}

/// A live comment of the project that `actor` may change, with its issue id
/// and stored body.
fn changeable(
    tx: &Transaction<'_>,
    actor: &Principal,
    project: &Project,
    comment_id: i64,
) -> Result<(Comment, i64, String), StoreError> {
    let (issue_id, stored): (i64, String) = tx
        .query_row(
            "SELECT c.issue_id, c.body FROM comment c
             JOIN issue i ON i.id = c.issue_id
             WHERE c.id = ?1 AND i.project_id = ?2 AND c.deleted IS NULL",
            params![comment_id, project.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| not_found("comment"))?;
    let comment = load_comment(tx, project, comment_id)?;
    if comment.author.id != actor.user_id && !actor.is_owner() {
        return Err(forbidden());
    }
    Ok((comment, issue_id, stored))
}

/// Replaces the body of a live comment. Only the author and the owner may.
/// An unchanged body is not an edit: nothing is written. Mentions are
/// recomputed; only users newly mentioned get a `mention.created` event.
pub fn edit_comment(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    comment_id: i64,
    body: &str,
) -> Result<Comment, StoreError> {
    let project = get_project(tx, project_key)?;
    let (before, issue_id, stored) = changeable(tx, actor, &project, comment_id)?;
    let body = clean_body(body)?;
    if body == stored {
        return Ok(before);
    }
    let mentions = resolve_mentions(tx, &body)?;
    let already: HashSet<i64> = before.mentions.iter().map(|m| m.user_id).collect();
    tx.execute(
        "UPDATE comment SET body = ?1, edited = ?2 WHERE id = ?3",
        params![body, now, comment_id],
    )?;
    store_mentions(tx, comment_id, &mentions)?;
    let comment = load_comment(tx, &project, comment_id)?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        issue_id,
        "comment.edited",
        &json!({
            "comment_id": comment_id,
            "mentions": mentions.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        }),
    )?;
    for user in mentions.iter().filter(|u| !already.contains(&u.id)) {
        emit_mention(tx, now, actor, &project, issue_id, &comment, user, &body)?;
    }
    Ok(comment)
}

/// Soft-deletes a live comment. Only the author and the owner may.
pub fn delete_comment(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    comment_id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project_key)?;
    let (_, issue_id, _) = changeable(tx, actor, &project, comment_id)?;
    tx.execute(
        "UPDATE comment SET deleted = ?1 WHERE id = ?2",
        params![now, comment_id],
    )?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        issue_id,
        "comment.deleted",
        &json!({ "comment_id": comment_id }),
    )
}
