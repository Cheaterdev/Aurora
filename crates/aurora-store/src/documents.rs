//! Documents: markdown pages of one project, nested under a parent, with an
//! append-only revision history, linked to issues and initiatives.
//!
//! Plain functions over a transaction (writes) or a connection (reads), like
//! [`crate::issues`]. A document belongs to one project and is addressed by
//! project key and id. Every write appends its event in the same transaction;
//! no event carries a body, only its size (see below).
//!
//! * Every create and every change of title or body appends a revision
//!   (number 1 on create); `document.revision` is the newest number. History
//!   is never rewritten: restoring revision N appends a new revision with N's
//!   title and body and the note `restored from N`. A write that changes
//!   neither title nor body writes nothing and emits no event.
//! * `base_revision` (update, restore) is optimistic concurrency: when given
//!   and not the current revision the write is a conflict.
//! * Parents: same project, not the document itself or below it (conflict),
//!   at most [`MAX_DEPTH`] levels, not archived. Siblings are ordered by
//!   `position` with the scheme of [`crate::issues`].
//! * Archiving is a soft delete. A document with live (non-archived) children
//!   cannot be archived (conflict) until they are moved or archived; an
//!   archived document cannot be edited, moved, restored or given links, and
//!   cannot come back while its parent is archived.
//! * Links tie a document to issues and initiatives of the same project.
//!   Deleting an initiative removes its document links (each with an event).

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::StoreError;
use crate::auth::Principal;
use crate::issues::present;
use crate::projects::{Project, conflict, emit, get_project, invalid, not_found};

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_BODY_CHARS: usize = 200_000;
/// The longest chain of parents, counting the top-level document and the deepest child.
pub const MAX_DEPTH: i64 = 8;
const POSITION_STEP: f64 = 1024.0;
/// Below this distance between two neighbours the siblings are renumbered.
const MIN_GAP: f64 = 1e-4;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Document {
    pub id: i64,
    pub project_id: i64,
    /// The parent document's id.
    pub parent: Option<i64>,
    pub title: String,
    /// Markdown.
    pub body: String,
    pub position: f64,
    /// The number of the newest revision.
    pub revision: i64,
    pub created: i64,
    pub updated: i64,
    pub created_by: Option<i64>,
    pub updated_by: Option<i64>,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentRef {
    pub id: i64,
    pub title: String,
}

/// A document as listed on an issue or an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkedDocument {
    pub id: i64,
    pub title: String,
    pub link_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkedIssue {
    pub link_id: i64,
    pub number: i64,
    pub identifier: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkedInitiative {
    pub link_id: i64,
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DocumentLinks {
    pub issues: Vec<LinkedIssue>,
    pub initiatives: Vec<LinkedInitiative>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocumentDetail {
    #[serde(flatten)]
    pub document: Document,
    /// From the top-level ancestor down to the parent.
    pub ancestors: Vec<DocumentRef>,
    /// The non-archived children, by position.
    pub children: Vec<DocumentRef>,
    pub links: DocumentLinks,
}

/// A row of the project's document list (no body).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocumentSummary {
    pub id: i64,
    pub parent: Option<i64>,
    pub title: String,
    pub position: f64,
    pub revision: i64,
    pub updated: i64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevisionSummary {
    pub number: i64,
    pub title: String,
    pub author_id: Option<i64>,
    pub author: Option<String>,
    pub created: i64,
    pub note: Option<String>,
    /// Characters in the body of this revision.
    pub chars: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Revision {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author_id: Option<i64>,
    pub author: Option<String>,
    pub created: i64,
    pub note: Option<String>,
}

/// The other end of a document link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentTarget {
    /// An issue number of the document's project.
    Issue(i64),
    /// An initiative id of the document's project.
    Initiative(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentLink {
    pub id: i64,
    pub document_id: i64,
    /// `issue` or `initiative`.
    pub kind: &'static str,
    pub issue: Option<LinkedIssue>,
    pub initiative: Option<LinkedInitiative>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDocument {
    pub title: String,
    pub body: Option<String>,
    /// Id of the parent document; none for a top-level document.
    pub parent: Option<i64>,
}

/// Only the fields that are present change; `parent: null` makes the
/// document top-level (a parent change puts it last among its new siblings).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub parent: Option<Option<i64>>,
    /// The revision the edit was made on; a different current revision is a conflict.
    pub base_revision: Option<i64>,
}

/// Where to put a document. `parent` absent keeps the parent, `null` means
/// top-level. At most one of `before`, `after`, `position` (document ids
/// among the new siblings); none means the end.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentMove {
    #[serde(default, deserialize_with = "present")]
    pub parent: Option<Option<i64>>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub position: Option<f64>,
}

const SELECT: &str = "SELECT id, project_id, parent_id, title, body, position, revision,
            created, updated, created_by, updated_by, archived
     FROM document";

fn document_row(row: &Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        project_id: row.get(1)?,
        parent: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        position: row.get(5)?,
        revision: row.get(6)?,
        created: row.get(7)?,
        updated: row.get(8)?,
        created_by: row.get(9)?,
        updated_by: row.get(10)?,
        archived: row.get(11)?,
    })
}

fn find(conn: &Connection, project_id: i64, id: i64) -> Result<Document, StoreError> {
    conn.query_row(
        &format!("{SELECT} WHERE id = ?1 AND project_id = ?2"),
        [id, project_id],
        document_row,
    )
    .optional()?
    .ok_or_else(|| not_found("document"))
}

fn clean_title(title: &str) -> Result<String, StoreError> {
    let title = title.trim();
    if (1..=MAX_TITLE_CHARS).contains(&title.chars().count()) {
        Ok(title.to_string())
    } else {
        Err(invalid(format!(
            "title must be 1-{MAX_TITLE_CHARS} characters"
        )))
    }
}

fn check_body(text: &str) -> Result<(), StoreError> {
    if text.chars().count() <= MAX_BODY_CHARS {
        Ok(())
    } else {
        Err(invalid(format!(
            "body must be at most {MAX_BODY_CHARS} characters"
        )))
    }
}

fn chars_of(text: &str) -> i64 {
    i64::try_from(text.chars().count()).unwrap_or(i64::MAX)
}

fn ensure_live(document: &Document) -> Result<(), StoreError> {
    if document.archived {
        Err(conflict("the document is archived; unarchive it first"))
    } else {
        Ok(())
    }
}

fn check_base(document: &Document, base: Option<i64>) -> Result<(), StoreError> {
    match base {
        Some(base) if base != document.revision => Err(conflict(format!(
            "the document is at revision {} now, not {base}: it was changed by someone else",
            document.revision
        ))),
        _ => Ok(()),
    }
}

// ---- tree rules ----

/// How many levels the chain from this document up to its top-level ancestor has (top-level = 1).
fn level_of(conn: &Connection, id: i64) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "WITH RECURSIVE up(id, parent_id, level) AS (
             SELECT id, parent_id, 1 FROM document WHERE id = ?1
             UNION ALL
             SELECT d.id, d.parent_id, up.level + 1 FROM document d JOIN up ON d.id = up.parent_id
         ) SELECT MAX(level) FROM up",
        [id],
        |row| row.get(0),
    )?)
}

/// How many levels the document and everything below it have (a leaf = 1).
fn height_of(conn: &Connection, id: i64) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "WITH RECURSIVE sub(id, depth) AS (
             SELECT ?1, 1
             UNION ALL
             SELECT d.id, sub.depth + 1 FROM document d JOIN sub ON d.parent_id = sub.id
         ) SELECT MAX(depth) FROM sub",
        [id],
        |row| row.get(0),
    )?)
}

/// Whether `ancestor` is `id` itself or one of its ancestors.
fn is_self_or_ancestor(conn: &Connection, ancestor: i64, id: i64) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "WITH RECURSIVE up(id, parent_id) AS (
                 SELECT id, parent_id FROM document WHERE id = ?1
                 UNION ALL
                 SELECT d.id, d.parent_id FROM document d JOIN up ON d.id = up.parent_id
             ) SELECT 1 FROM up WHERE id = ?2",
            [id, ancestor],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Applies the parent rules. `child` is the document being re-parented;
/// `None` when it is being created.
fn check_parent(
    conn: &Connection,
    project: &Project,
    parent_id: i64,
    child: Option<i64>,
) -> Result<(), StoreError> {
    let parent = find(conn, project.id, parent_id).map_err(|err| match err {
        StoreError::Rejected(crate::Rejection::NotFound(_)) => invalid(format!(
            "parent {parent_id} is not a document of project {}",
            project.key
        )),
        other => other,
    })?;
    if let Some(child_id) = child
        && is_self_or_ancestor(conn, child_id, parent_id)?
    {
        return Err(conflict(format!(
            "document {parent_id} cannot be the parent: it is the document itself or below it"
        )));
    }
    if parent.archived {
        return Err(conflict(format!(
            "document {parent_id} is archived and cannot hold documents"
        )));
    }
    let below = match child {
        Some(child_id) => height_of(conn, child_id)?,
        None => 1,
    };
    if level_of(conn, parent_id)? + below > MAX_DEPTH {
        return Err(invalid(format!(
            "documents can be nested at most {MAX_DEPTH} levels deep"
        )));
    }
    Ok(())
}

// ---- positions ----

/// The siblings under `parent` (archived ones too) except `skip`, as (id, position).
fn siblings(
    conn: &Connection,
    project_id: i64,
    parent: Option<i64>,
    skip: Option<i64>,
) -> Result<Vec<(i64, f64)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, position FROM document
         WHERE project_id = ?1 AND parent_id IS ?2 ORDER BY position, id",
    )?;
    let rows = stmt
        .query_map(params![project_id, parent], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().filter(|r| Some(r.0) != skip).collect())
}

fn end_position(
    conn: &Connection,
    project_id: i64,
    parent: Option<i64>,
) -> Result<f64, StoreError> {
    let last: f64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) FROM document
         WHERE project_id = ?1 AND parent_id IS ?2",
        params![project_id, parent],
        |row| row.get(0),
    )?;
    Ok(last + POSITION_STEP)
}

#[derive(Clone, Copy)]
enum Place {
    End,
    Before(i64),
    After(i64),
    At(f64),
}

fn renumber(tx: &Transaction<'_>, order: &mut [(i64, f64)]) -> Result<(), StoreError> {
    for ((id, position), step) in order.iter_mut().zip(1_u32..) {
        let wanted = POSITION_STEP * f64::from(step);
        if *position != wanted {
            tx.execute(
                "UPDATE document SET position = ?1 WHERE id = ?2",
                params![wanted, *id],
            )?;
            *position = wanted;
        }
    }
    Ok(())
}

/// A position for `place` among `others`; renumbers them first when the room
/// between the two neighbours has run out. Returns whether it did.
fn position_for(
    tx: &Transaction<'_>,
    others: &mut [(i64, f64)],
    place: Place,
) -> Result<(f64, bool), StoreError> {
    let mut renumbered = false;
    loop {
        let index_of = |anchor: i64| others.iter().position(|o| o.0 == anchor);
        let (low, high) = match place {
            Place::At(position) => return Ok((position, false)),
            Place::End => (others.last().map(|o| o.1), None),
            Place::Before(anchor) => {
                let at = index_of(anchor);
                (
                    at.and_then(|i| i.checked_sub(1)).map(|i| others[i].1),
                    at.map(|i| others[i].1),
                )
            }
            Place::After(anchor) => {
                let at = index_of(anchor);
                (
                    at.map(|i| others[i].1),
                    at.and_then(|i| others.get(i + 1)).map(|o| o.1),
                )
            }
        };
        return Ok(match (low, high) {
            (Some(low), Some(high)) if high - low < MIN_GAP && !renumbered => {
                renumber(tx, others)?;
                renumbered = true;
                continue;
            }
            (Some(low), Some(high)) => ((low + high) / 2.0, renumbered),
            (None, Some(high)) => (high - POSITION_STEP, renumbered),
            (Some(low), None) => (low + POSITION_STEP, renumbered),
            (None, None) => (POSITION_STEP, renumbered),
        });
    }
}

// ---- reads ----

fn links_of(conn: &Connection, key: &str, document_id: i64) -> Result<DocumentLinks, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT l.id, i.number, i.title
         FROM document_link l JOIN issue i ON i.id = l.issue_id
         WHERE l.document_id = ?1 ORDER BY l.id",
    )?;
    let issues = stmt
        .query_map([document_id], |row| {
            let number: i64 = row.get(1)?;
            Ok(LinkedIssue {
                link_id: row.get(0)?,
                number,
                identifier: format!("{key}-{number}"),
                title: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT l.id, n.id, n.name
         FROM document_link l JOIN initiative n ON n.id = l.initiative_id
         WHERE l.document_id = ?1 ORDER BY l.id",
    )?;
    let initiatives = stmt
        .query_map([document_id], |row| {
            Ok(LinkedInitiative {
                link_id: row.get(0)?,
                id: row.get(1)?,
                name: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DocumentLinks {
        issues,
        initiatives,
    })
}

fn detail(conn: &Connection, project: &Project, id: i64) -> Result<DocumentDetail, StoreError> {
    let document = find(conn, project.id, id)?;
    let mut stmt = conn.prepare(
        "WITH RECURSIVE up(id, title, parent_id, depth) AS (
             SELECT p.id, p.title, p.parent_id, 1 FROM document d
               JOIN document p ON p.id = d.parent_id WHERE d.id = ?1
             UNION ALL
             SELECT p.id, p.title, p.parent_id, up.depth + 1 FROM document p
               JOIN up ON p.id = up.parent_id
         ) SELECT id, title FROM up ORDER BY depth DESC",
    )?;
    let ancestors = stmt
        .query_map([id], |row| {
            Ok(DocumentRef {
                id: row.get(0)?,
                title: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT id, title FROM document WHERE parent_id = ?1 AND archived = 0
         ORDER BY position, id",
    )?;
    let children = stmt
        .query_map([id], |row| {
            Ok(DocumentRef {
                id: row.get(0)?,
                title: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let links = links_of(conn, &project.key, id)?;
    Ok(DocumentDetail {
        document,
        ancestors,
        children,
        links,
    })
}

pub fn get_document(
    conn: &Connection,
    project_key: &str,
    id: i64,
) -> Result<DocumentDetail, StoreError> {
    let project = get_project(conn, project_key)?;
    detail(conn, &project, id)
}

/// Flat and without bodies, ordered by parent, then position: build the tree
/// with [`in_tree_order`]. Archived documents only when asked for.
pub fn list_documents(
    conn: &Connection,
    project_key: &str,
    include_archived: bool,
) -> Result<Vec<DocumentSummary>, StoreError> {
    let project = get_project(conn, project_key)?;
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, title, position, revision, updated, archived FROM document
         WHERE project_id = ?1 AND (?2 OR archived = 0)
         ORDER BY parent_id IS NOT NULL, parent_id, position, id",
    )?;
    let rows = stmt
        .query_map(params![project.id, include_archived], |row| {
            Ok(DocumentSummary {
                id: row.get(0)?,
                parent: row.get(1)?,
                title: row.get(2)?,
                position: row.get(3)?,
                revision: row.get(4)?,
                updated: row.get(5)?,
                archived: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The list as a depth-first walk: `(depth, document)`, top-level documents
/// at depth 0, each followed by its descendants. A document whose parent is
/// not in the list counts as top-level, so none is ever lost.
pub fn in_tree_order(list: Vec<DocumentSummary>) -> Vec<(usize, DocumentSummary)> {
    let present: std::collections::HashSet<i64> = list.iter().map(|d| d.id).collect();
    let mut children: std::collections::HashMap<Option<i64>, Vec<DocumentSummary>> =
        std::collections::HashMap::new();
    for document in list {
        let parent = document.parent.filter(|p| present.contains(p));
        children.entry(parent).or_default().push(document);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| a.position.total_cmp(&b.position).then(a.id.cmp(&b.id)));
        siblings.reverse();
    }
    let mut out = Vec::new();
    let mut pending: Vec<(usize, DocumentSummary)> = children
        .remove(&None)
        .unwrap_or_default()
        .into_iter()
        .map(|d| (0, d))
        .collect();
    while let Some((depth, document)) = pending.pop() {
        if let Some(below) = children.remove(&Some(document.id)) {
            pending.extend(below.into_iter().map(|d| (depth + 1, d)));
        }
        out.push((depth, document));
    }
    out
}

/// Newest first.
pub fn list_revisions(
    conn: &Connection,
    project_key: &str,
    id: i64,
) -> Result<Vec<RevisionSummary>, StoreError> {
    let project = get_project(conn, project_key)?;
    find(conn, project.id, id)?;
    let mut stmt = conn.prepare(
        "SELECT r.number, r.title, r.author_id, u.name, r.created, r.note, LENGTH(r.body)
         FROM document_revision r LEFT JOIN user u ON u.id = r.author_id
         WHERE r.document_id = ?1 ORDER BY r.number DESC",
    )?;
    let rows = stmt
        .query_map([id], |row| {
            Ok(RevisionSummary {
                number: row.get(0)?,
                title: row.get(1)?,
                author_id: row.get(2)?,
                author: row.get(3)?,
                created: row.get(4)?,
                note: row.get(5)?,
                chars: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn revision_of(conn: &Connection, document_id: i64, number: i64) -> Result<Revision, StoreError> {
    conn.query_row(
        "SELECT r.number, r.title, r.body, r.author_id, u.name, r.created, r.note
         FROM document_revision r LEFT JOIN user u ON u.id = r.author_id
         WHERE r.document_id = ?1 AND r.number = ?2",
        [document_id, number],
        |row| {
            Ok(Revision {
                number: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                author_id: row.get(3)?,
                author: row.get(4)?,
                created: row.get(5)?,
                note: row.get(6)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| not_found("revision"))
}

pub fn get_revision(
    conn: &Connection,
    project_key: &str,
    id: i64,
    number: i64,
) -> Result<Revision, StoreError> {
    let project = get_project(conn, project_key)?;
    find(conn, project.id, id)?;
    revision_of(conn, id, number)
}

/// The non-archived documents linked to an issue, in the order linked; one query.
pub(crate) fn documents_of_issue(
    conn: &Connection,
    issue_id: i64,
) -> Result<Vec<LinkedDocument>, StoreError> {
    linked_documents(conn, "issue_id", issue_id)
}

pub(crate) fn documents_of_initiative(
    conn: &Connection,
    initiative_id: i64,
) -> Result<Vec<LinkedDocument>, StoreError> {
    linked_documents(conn, "initiative_id", initiative_id)
}

fn linked_documents(
    conn: &Connection,
    column: &str,
    target_id: i64,
) -> Result<Vec<LinkedDocument>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT d.id, d.title, l.id FROM document_link l
         JOIN document d ON d.id = l.document_id
         WHERE l.{column} = ?1 AND d.archived = 0 ORDER BY l.id"
    ))?;
    let rows = stmt
        .query_map([target_id], |row| {
            Ok(LinkedDocument {
                id: row.get(0)?,
                title: row.get(1)?,
                link_id: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---- writes ----

#[allow(clippy::too_many_arguments)]
fn append_revision(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    document_id: i64,
    number: i64,
    title: &str,
    body: &str,
    note: Option<&str>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO document_revision (document_id, number, title, body, author_id, created, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![document_id, number, title, body, actor.user_id, now, note],
    )?;
    Ok(())
}

pub fn create_document(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    new: NewDocument,
) -> Result<DocumentDetail, StoreError> {
    let title = clean_title(&new.title)?;
    let body = new.body.unwrap_or_default();
    check_body(&body)?;
    let project = get_project(tx, project_key)?;
    if let Some(parent) = new.parent {
        check_parent(tx, &project, parent, None)?;
    }
    let position = end_position(tx, project.id, new.parent)?;
    tx.execute(
        "INSERT INTO document (project_id, parent_id, title, body, position, revision,
                               created, updated, created_by, updated_by)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6, ?7, ?7)",
        params![
            project.id,
            new.parent,
            title,
            body,
            position,
            now,
            actor.user_id
        ],
    )?;
    let id = tx.last_insert_rowid();
    append_revision(tx, now, actor, id, 1, &title, &body, None)?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "document",
        id,
        "document.created",
        &json!({
            "title": title,
            "parent": new.parent,
            "revision": 1,
            "chars": chars_of(&body),
        }),
    )?;
    detail(tx, &project, id)
}

#[allow(clippy::too_many_arguments)]
fn emit_moved(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_id: i64,
    id: i64,
    from: Option<i64>,
    to: Option<i64>,
    position: f64,
) -> Result<(), StoreError> {
    emit(
        tx,
        now,
        actor,
        project_id,
        "document",
        id,
        "document.moved",
        &json!({ "from_parent": from, "to_parent": to, "position": position }),
    )
}

/// A write that changes nothing writes nothing and emits no event.
pub fn update_document(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    patch: DocumentPatch,
) -> Result<DocumentDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, project.id, id)?;
    ensure_live(&current)?;
    check_base(&current, patch.base_revision)?;

    let title = match &patch.title {
        Some(title) => clean_title(title)?,
        None => current.title.clone(),
    };
    let body = match &patch.body {
        Some(body) => {
            check_body(body)?;
            body.clone()
        }
        None => current.body.clone(),
    };
    let new_parent = patch.parent.filter(|parent| *parent != current.parent);
    if let Some(Some(parent)) = new_parent {
        check_parent(tx, &project, parent, Some(id))?;
    }
    let title_changed = title != current.title;
    let body_changed = body != current.body;

    if !title_changed && !body_changed && new_parent.is_none() {
        return detail(tx, &project, id);
    }

    if title_changed || body_changed {
        let revision = current.revision + 1;
        tx.execute(
            "UPDATE document SET title = ?1, body = ?2, revision = ?3, updated = ?4, updated_by = ?5
             WHERE id = ?6",
            params![title, body, revision, now, actor.user_id, id],
        )?;
        append_revision(tx, now, actor, id, revision, &title, &body, None)?;
        let mut changes = Map::new();
        if title_changed {
            changes.insert(
                "title".to_string(),
                json!({ "from": current.title, "to": title }),
            );
        }
        emit(
            tx,
            now,
            actor,
            project.id,
            "document",
            id,
            "document.updated",
            &json!({
                "revision": revision,
                "body_changed": body_changed,
                "chars": chars_of(&body),
                "changes": changes,
            }),
        )?;
    }
    if let Some(parent) = new_parent {
        let position = end_position(tx, project.id, parent)?;
        tx.execute(
            "UPDATE document SET parent_id = ?1, position = ?2, updated = ?3, updated_by = ?4
             WHERE id = ?5",
            params![parent, position, now, actor.user_id, id],
        )?;
        emit_moved(
            tx,
            now,
            actor,
            project.id,
            id,
            current.parent,
            parent,
            position,
        )?;
    }
    detail(tx, &project, id)
}

/// Changes the parent and/or the place among the siblings. Moving to where
/// the document already is changes nothing and writes no event.
pub fn move_document(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    target: DocumentMove,
) -> Result<DocumentDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, project.id, id)?;
    ensure_live(&current)?;
    let given = [
        target.before.is_some(),
        target.after.is_some(),
        target.position.is_some(),
    ];
    if given.into_iter().filter(|g| *g).count() > 1 {
        return Err(invalid("give at most one of before, after and position"));
    }
    if target.position.is_some_and(|p| !p.is_finite()) {
        return Err(invalid("position must be a finite number"));
    }
    let parent = target.parent.unwrap_or(current.parent);
    if parent != current.parent
        && let Some(parent) = parent
    {
        check_parent(tx, &project, parent, Some(id))?;
    }
    if target.before == Some(id) || target.after == Some(id) {
        return Err(invalid("a document cannot be placed relative to itself"));
    }

    let mut others = siblings(tx, project.id, parent, Some(id))?;
    let anchor = |wanted: i64| -> Result<i64, StoreError> {
        if others.iter().any(|o| o.0 == wanted) {
            Ok(wanted)
        } else {
            Err(invalid(format!(
                "document {wanted} is not among the documents it would sit beside"
            )))
        }
    };
    let place = match (target.before, target.after, target.position) {
        (Some(before), _, _) => Place::Before(anchor(before)?),
        (_, Some(after), _) => Place::After(anchor(after)?),
        (_, _, Some(position)) => Place::At(position),
        _ => Place::End,
    };

    if parent == current.parent {
        let full = siblings(tx, project.id, parent, None)?;
        let at = full.iter().position(|o| o.0 == id);
        let already = match place {
            Place::End => at.is_some_and(|i| i + 1 == full.len()),
            Place::Before(anchor) => at
                .and_then(|i| full.get(i + 1))
                .is_some_and(|o| o.0 == anchor),
            Place::After(anchor) => at
                .and_then(|i| i.checked_sub(1))
                .is_some_and(|i| full[i].0 == anchor),
            Place::At(position) => position == current.position,
        };
        if already {
            return detail(tx, &project, id);
        }
    }

    let (position, _) = position_for(tx, &mut others, place)?;
    tx.execute(
        "UPDATE document SET parent_id = ?1, position = ?2, updated = ?3, updated_by = ?4
         WHERE id = ?5",
        params![parent, position, now, actor.user_id, id],
    )?;
    emit_moved(
        tx,
        now,
        actor,
        project.id,
        id,
        current.parent,
        parent,
        position,
    )?;
    detail(tx, &project, id)
}

fn set_archived(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    archived: bool,
) -> Result<DocumentDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, project.id, id)?;
    if current.archived == archived {
        return detail(tx, &project, id);
    }
    if archived {
        let live: i64 = tx.query_row(
            "SELECT COUNT(*) FROM document WHERE parent_id = ?1 AND archived = 0",
            [id],
            |row| row.get(0),
        )?;
        if live > 0 {
            return Err(conflict(format!(
                "the document has {live} child document(s): move or archive them first"
            )));
        }
    } else if let Some(parent) = current.parent
        && find(tx, project.id, parent)?.archived
    {
        return Err(conflict(
            "the parent document is archived: unarchive it first or move this one",
        ));
    }
    tx.execute(
        "UPDATE document SET archived = ?1, updated = ?2, updated_by = ?3 WHERE id = ?4",
        params![archived, now, actor.user_id, id],
    )?;
    let kind = if archived {
        "document.archived"
    } else {
        "document.unarchived"
    };
    emit(
        tx,
        now,
        actor,
        project.id,
        "document",
        id,
        kind,
        &json!({ "title": current.title }),
    )?;
    detail(tx, &project, id)
}

pub fn archive_document(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
) -> Result<DocumentDetail, StoreError> {
    set_archived(tx, now, actor, project_key, id, true)
}

pub fn unarchive_document(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
) -> Result<DocumentDetail, StoreError> {
    set_archived(tx, now, actor, project_key, id, false)
}

/// Appends a new revision with the title and body of revision `number`
/// (note `restored from N`); the history before it is untouched. Restoring
/// what the document already says writes nothing.
pub fn restore_revision(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    number: i64,
    base_revision: Option<i64>,
) -> Result<DocumentDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, project.id, id)?;
    ensure_live(&current)?;
    check_base(&current, base_revision)?;
    let old = revision_of(tx, id, number)?;
    if old.title == current.title && old.body == current.body {
        return detail(tx, &project, id);
    }
    let revision = current.revision + 1;
    tx.execute(
        "UPDATE document SET title = ?1, body = ?2, revision = ?3, updated = ?4, updated_by = ?5
         WHERE id = ?6",
        params![old.title, old.body, revision, now, actor.user_id, id],
    )?;
    let note = format!("restored from {number}");
    append_revision(
        tx,
        now,
        actor,
        id,
        revision,
        &old.title,
        &old.body,
        Some(&note),
    )?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "document",
        id,
        "document.restored",
        &json!({
            "from_revision": number,
            "revision": revision,
            "title": old.title,
            "chars": chars_of(&old.body),
        }),
    )?;
    detail(tx, &project, id)
}

// ---- links ----

fn target_json(target: DocumentTarget) -> Value {
    match target {
        DocumentTarget::Issue(number) => json!({ "kind": "issue", "number": number }),
        DocumentTarget::Initiative(id) => json!({ "kind": "initiative", "id": id }),
    }
}

/// The (issue id, initiative id) pair of a link row.
type TargetIds = (Option<i64>, Option<i64>);

/// The pair a target stands for, when the target is in the project.
fn resolve_target(
    conn: &Connection,
    project: &Project,
    target: DocumentTarget,
) -> Result<Option<TargetIds>, StoreError> {
    Ok(match target {
        DocumentTarget::Issue(number) => conn
            .query_row(
                "SELECT id FROM issue WHERE project_id = ?1 AND number = ?2",
                [project.id, number],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|id| (Some(id), None)),
        DocumentTarget::Initiative(initiative) => conn
            .query_row(
                "SELECT id FROM initiative WHERE project_id = ?1 AND id = ?2",
                [project.id, initiative],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|id| (None, Some(id))),
    })
}

fn link_row(
    conn: &Connection,
    key: &str,
    document_id: i64,
    link_id: i64,
) -> Result<DocumentLink, StoreError> {
    let links = links_of(conn, key, document_id)?;
    if let Some(issue) = links.issues.into_iter().find(|l| l.link_id == link_id) {
        return Ok(DocumentLink {
            id: link_id,
            document_id,
            kind: "issue",
            issue: Some(issue),
            initiative: None,
        });
    }
    links
        .initiatives
        .into_iter()
        .find(|l| l.link_id == link_id)
        .map(|initiative| DocumentLink {
            id: link_id,
            document_id,
            kind: "initiative",
            issue: None,
            initiative: Some(initiative),
        })
        .ok_or_else(|| not_found("link"))
}

/// Links the document to an issue (by number) or an initiative (by id) of
/// its project. Refused: a target of another project or none (invalid) and a
/// link that exists already (conflict).
pub fn add_link(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    target: DocumentTarget,
) -> Result<DocumentLink, StoreError> {
    let project = get_project(tx, project_key)?;
    let document = find(tx, project.id, id)?;
    ensure_live(&document)?;
    let (issue_id, initiative_id) = resolve_target(tx, &project, target)?.ok_or_else(|| {
        invalid(match target {
            DocumentTarget::Issue(number) => {
                format!("issue {number} not found in project {}", project.key)
            }
            DocumentTarget::Initiative(initiative) => format!(
                "initiative {initiative} is not an initiative of project {}",
                project.key
            ),
        })
    })?;
    let exists = tx
        .query_row(
            "SELECT 1 FROM document_link
             WHERE document_id = ?1 AND (issue_id IS ?2 AND initiative_id IS ?3)",
            params![id, issue_id, initiative_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Err(conflict("the document is already linked to that target"));
    }
    tx.execute(
        "INSERT INTO document_link (document_id, issue_id, initiative_id, created, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, issue_id, initiative_id, now, actor.user_id],
    )?;
    let link_id = tx.last_insert_rowid();
    emit(
        tx,
        now,
        actor,
        project.id,
        "document",
        id,
        "document.link_added",
        &json!({ "link_id": link_id, "target": target_json(target) }),
    )?;
    link_row(tx, &project.key, id, link_id)
}

/// The target of a link as an event describes it.
fn stored_target(conn: &Connection, link_id: i64) -> Result<DocumentTarget, StoreError> {
    conn.query_row(
        "SELECT i.number, l.initiative_id FROM document_link l
         LEFT JOIN issue i ON i.id = l.issue_id WHERE l.id = ?1",
        [link_id],
        |row| {
            let number: Option<i64> = row.get(0)?;
            let initiative: Option<i64> = row.get(1)?;
            Ok(match (number, initiative) {
                (Some(number), _) => DocumentTarget::Issue(number),
                (None, initiative) => DocumentTarget::Initiative(initiative.unwrap_or_default()),
            })
        },
    )
    .map_err(Into::into)
}

/// Removes a link of the document; a link of another document is not found.
pub fn remove_link(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    link_id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project_key)?;
    find(tx, project.id, id)?;
    let owned = tx
        .query_row(
            "SELECT 1 FROM document_link WHERE id = ?1 AND document_id = ?2",
            [link_id, id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !owned {
        return Err(not_found("link"));
    }
    let target = stored_target(tx, link_id)?;
    tx.execute("DELETE FROM document_link WHERE id = ?1", [link_id])?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "document",
        id,
        "document.link_removed",
        &json!({ "link_id": link_id, "target": target_json(target) }),
    )
}

/// Removes the link from the document to `target`, whichever link id it has.
pub fn remove_link_to(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    target: DocumentTarget,
) -> Result<(), StoreError> {
    let project = get_project(tx, project_key)?;
    find(tx, project.id, id)?;
    let (issue_id, initiative_id) =
        resolve_target(tx, &project, target)?.ok_or_else(|| not_found("link"))?;
    let link_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM document_link
             WHERE document_id = ?1 AND issue_id IS ?2 AND initiative_id IS ?3",
            params![id, issue_id, initiative_id],
            |row| row.get(0),
        )
        .optional()?;
    let link_id = link_id.ok_or_else(|| not_found("link"))?;
    remove_link(tx, now, actor, project_key, id, link_id)
}

/// Drops every document link of an initiative that is being deleted, each
/// with its own `document.link_removed` event, in the caller's transaction.
pub(crate) fn unlink_initiative(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_id: i64,
    initiative_id: i64,
) -> Result<(), StoreError> {
    let mut stmt = tx.prepare(
        "SELECT id, document_id FROM document_link WHERE initiative_id = ?1 ORDER BY id",
    )?;
    let found = stmt
        .query_map([initiative_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (link_id, document_id) in found {
        tx.execute("DELETE FROM document_link WHERE id = ?1", [link_id])?;
        emit(
            tx,
            now,
            actor,
            project_id,
            "document",
            document_id,
            "document.link_removed",
            &json!({
                "link_id": link_id,
                "target": target_json(DocumentTarget::Initiative(initiative_id)),
            }),
        )?;
    }
    Ok(())
}
