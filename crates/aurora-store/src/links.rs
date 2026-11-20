//! Links between issues of one project: `blocks` (directed) and `relates`
//! (symmetric).
//!
//! Plain functions over a transaction (writes) or a connection (reads), like
//! [`crate::issues`]. Every change appends its event in the same transaction.
//! Issues are addressed by project key and number; links by id.
//!
//! A `relates` link is stored once with the lower issue id first, so the same
//! pair cannot be linked twice in opposite directions. An issue is *blocked*
//! while at least one issue that blocks it is not archived and sits in a column
//! whose state type is neither completed nor canceled.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::StoreError;
use crate::auth::Principal;
use crate::projects::{Project, StateType, conflict, emit, get_project, invalid, not_found};

/// How many issues the cycle check visits before it gives up. A chain this
/// large is refused rather than walked.
pub const MAX_WALK: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Blocks,
    Relates,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Blocks => "blocks",
            LinkKind::Relates => "relates",
        }
    }

    fn parse(text: &str) -> Option<LinkKind> {
        match text {
            "blocks" => Some(LinkKind::Blocks),
            "relates" => Some(LinkKind::Relates),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueRef {
    pub number: i64,
    pub identifier: String,
}

/// A stored link. For `relates`, `from` is the issue with the lower id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub id: i64,
    pub kind: LinkKind,
    pub from: IssueRef,
    pub to: IssueRef,
    pub created: i64,
    pub created_by: Option<i64>,
}

/// The other end of a link, seen from one issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkedIssue {
    pub link_id: i64,
    pub number: i64,
    pub identifier: String,
    pub title: String,
    pub column_id: i64,
    pub state_type: StateType,
    pub archived: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IssueLinks {
    /// Issues this one blocks.
    pub blocks: Vec<LinkedIssue>,
    /// Issues blocking this one.
    pub blocked_by: Vec<LinkedIssue>,
    pub related: Vec<LinkedIssue>,
}

/// An open blocker of a blocked issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    pub number: i64,
    pub identifier: String,
    pub title: String,
}

fn issue_by_number(
    conn: &Connection,
    project: &Project,
    number: i64,
) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM issue WHERE project_id = ?1 AND number = ?2",
            [project.id, number],
            |row| row.get(0),
        )
        .optional()?)
}

fn link_row(row: &Row<'_>, key: &str) -> rusqlite::Result<Link> {
    let kind: String = row.get(1)?;
    let (from, to): (i64, i64) = (row.get(2)?, row.get(3)?);
    Ok(Link {
        id: row.get(0)?,
        kind: LinkKind::parse(&kind).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(1, kind, rusqlite::types::Type::Text)
        })?,
        from: IssueRef {
            number: from,
            identifier: format!("{key}-{from}"),
        },
        to: IssueRef {
            number: to,
            identifier: format!("{key}-{to}"),
        },
        created: row.get(4)?,
        created_by: row.get(5)?,
    })
}

const LINK_SELECT: &str = "SELECT l.id, l.kind, f.number, t.number, l.created, l.created_by
     FROM issue_link l
     JOIN issue f ON f.id = l.from_issue_id
     JOIN issue t ON t.id = l.to_issue_id";

/// Whether `target` can be reached from `start` over `blocks` links.
fn blocks_reach(conn: &Connection, start: i64, target: i64) -> Result<bool, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT to_issue_id FROM issue_link WHERE kind = 'blocks' AND from_issue_id = ?1",
    )?;
    let mut seen = HashSet::from([start]);
    let mut pending = vec![start];
    while let Some(id) = pending.pop() {
        let next = stmt
            .query_map([id], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for issue in next {
            if issue == target {
                return Ok(true);
            }
            if seen.insert(issue) {
                if seen.len() > MAX_WALK {
                    return Err(invalid(
                        "the chain of blocking issues is too long to check for cycles",
                    ));
                }
                pending.push(issue);
            }
        }
    }
    Ok(false)
}

// ---- reads ----

/// The links of one issue, each group ordered by link id.
pub(crate) fn links_of(
    conn: &Connection,
    key: &str,
    issue_id: i64,
) -> Result<IssueLinks, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT l.id, l.kind, 1, o.number, o.title, o.column_id, c.state_type, o.archived
         FROM issue_link l
         JOIN issue o ON o.id = l.to_issue_id
         JOIN \"column\" c ON c.id = o.column_id
         WHERE l.from_issue_id = ?1
         UNION ALL
         SELECT l.id, l.kind, 0, o.number, o.title, o.column_id, c.state_type, o.archived
         FROM issue_link l
         JOIN issue o ON o.id = l.from_issue_id
         JOIN \"column\" c ON c.id = o.column_id
         WHERE l.to_issue_id = ?1
         ORDER BY 1",
    )?;
    let rows = stmt
        .query_map([issue_id], |row| {
            let number: i64 = row.get(3)?;
            let kind: String = row.get(1)?;
            let outgoing: bool = row.get(2)?;
            Ok((
                kind,
                outgoing,
                LinkedIssue {
                    link_id: row.get(0)?,
                    number,
                    identifier: format!("{key}-{number}"),
                    title: row.get(4)?,
                    column_id: row.get(5)?,
                    state_type: row.get(6)?,
                    archived: row.get(7)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut links = IssueLinks::default();
    for (kind, outgoing, issue) in rows {
        match (kind.as_str(), outgoing) {
            ("blocks", true) => links.blocks.push(issue),
            ("blocks", false) => links.blocked_by.push(issue),
            _ => links.related.push(issue),
        }
    }
    Ok(links)
}

pub fn list_links(
    conn: &Connection,
    project_key: &str,
    number: i64,
) -> Result<IssueLinks, StoreError> {
    let project = get_project(conn, project_key)?;
    let id = issue_by_number(conn, &project, number)?.ok_or_else(|| not_found("issue"))?;
    links_of(conn, &project.key, id)
}

/// The open blockers of every blocked issue of the project, by issue id, in
/// one query. Issues that are not blocked are absent.
pub fn open_blockers(
    conn: &Connection,
    project_key: &str,
) -> Result<HashMap<i64, Vec<Blocker>>, StoreError> {
    let project = get_project(conn, project_key)?;
    let mut stmt = conn.prepare(concat!(
        "SELECT l.to_issue_id, b.number, b.title
         FROM issue_link l
         JOIN issue b ON b.id = l.from_issue_id
         JOIN \"column\" bc ON bc.id = b.column_id
         JOIN issue t ON t.id = l.to_issue_id
         WHERE l.kind = 'blocks' AND t.project_id = ?1 AND ",
        open_blocker!(),
        " ORDER BY l.to_issue_id, b.number"
    ))?;
    let rows = stmt
        .query_map([project.id], |row| {
            let number: i64 = row.get(1)?;
            Ok((
                row.get::<_, i64>(0)?,
                Blocker {
                    number,
                    identifier: format!("{}-{number}", project.key),
                    title: row.get(2)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut found: HashMap<i64, Vec<Blocker>> = HashMap::new();
    for (issue, blocker) in rows {
        found.entry(issue).or_default().push(blocker);
    }
    Ok(found)
}

// ---- writes ----

/// Links `from` to `to`. For `blocks`, `from` blocks `to`; for `relates` the
/// order does not matter. Refused: an issue linked to itself (invalid), an
/// issue that is not in the project (invalid), a link that exists already
/// (conflict) and a `blocks` link that would close a cycle (conflict).
pub fn add_link(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    from_number: i64,
    to_number: i64,
    kind: LinkKind,
) -> Result<Link, StoreError> {
    link_issues(
        tx,
        now,
        actor,
        project_key,
        from_number,
        to_number,
        kind,
        false,
    )
}

/// `add_link(blocker, this, Blocks)` for a caller that knows `this` exists and
/// treats `blocker` as the thing it was told about: an unknown `this` is
/// not found, an unknown `blocker` is invalid.
pub fn add_blocker(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    this_number: i64,
    blocker_number: i64,
) -> Result<Link, StoreError> {
    link_issues(
        tx,
        now,
        actor,
        project_key,
        this_number,
        blocker_number,
        LinkKind::Blocks,
        true,
    )
}

/// `subject` is the issue the request is about (not found when missing),
/// `other` the one it names (invalid when missing). `reversed` makes `other`
/// the `from` end.
#[allow(clippy::too_many_arguments)]
fn link_issues(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    subject_number: i64,
    other_number: i64,
    kind: LinkKind,
    reversed: bool,
) -> Result<Link, StoreError> {
    let project = get_project(tx, project_key)?;
    let subject_id =
        issue_by_number(tx, &project, subject_number)?.ok_or_else(|| not_found("issue"))?;
    let other_id = issue_by_number(tx, &project, other_number)?.ok_or_else(|| {
        invalid(format!(
            "issue {other_number} not found in project {}",
            project.key
        ))
    })?;
    let ((from_id, from_number), (to_id, to_number)) = if reversed {
        ((other_id, other_number), (subject_id, subject_number))
    } else {
        ((subject_id, subject_number), (other_id, other_number))
    };
    if from_id == to_id {
        return Err(invalid("an issue cannot be linked to itself"));
    }
    let (from_id, to_id, from_number, to_number) = if kind == LinkKind::Relates && from_id > to_id {
        (to_id, from_id, to_number, from_number)
    } else {
        (from_id, to_id, from_number, to_number)
    };
    let exists = tx
        .query_row(
            "SELECT 1 FROM issue_link WHERE kind = ?1 AND from_issue_id = ?2 AND to_issue_id = ?3",
            params![kind.as_str(), from_id, to_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Err(conflict(format!(
            "{key}-{from_number} and {key}-{to_number} are already linked ({kind})",
            key = project.key,
            kind = kind.as_str()
        )));
    }
    if kind == LinkKind::Blocks && blocks_reach(tx, to_id, from_id)? {
        return Err(conflict(format!(
            "{key}-{to_number} already blocks {key}-{from_number}, directly or through other \
             issues: this link would make a cycle",
            key = project.key
        )));
    }
    tx.execute(
        "INSERT INTO issue_link (from_issue_id, to_issue_id, kind, created, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from_id, to_id, kind.as_str(), now, actor.user_id],
    )?;
    let id = tx.last_insert_rowid();
    let link = tx.query_row(&format!("{LINK_SELECT} WHERE l.id = ?1"), [id], |row| {
        link_row(row, &project.key)
    })?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        from_id,
        "link.added",
        &event_payload(&link),
    )?;
    Ok(link)
}

fn event_payload(link: &Link) -> serde_json::Value {
    json!({
        "link_id": link.id,
        "kind": link.kind.as_str(),
        "from": { "number": link.from.number, "identifier": link.from.identifier },
        "to": { "number": link.to.number, "identifier": link.to.identifier },
    })
}

/// Removes a link of the project. Archived issues keep theirs, so this is
/// the only way a link goes away.
pub fn remove_link(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    link_id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project_key)?;
    let found = tx
        .query_row(
            &format!("{LINK_SELECT} WHERE l.id = ?1 AND f.project_id = ?2"),
            params![link_id, project.id],
            |row| link_row(row, &project.key),
        )
        .optional()?
        .ok_or_else(|| not_found("link"))?;
    let from_id: i64 = tx.query_row(
        "SELECT from_issue_id FROM issue_link WHERE id = ?1",
        [link_id],
        |row| row.get(0),
    )?;
    tx.execute("DELETE FROM issue_link WHERE id = ?1", [link_id])?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        from_id,
        "link.removed",
        &event_payload(&found),
    )
}
