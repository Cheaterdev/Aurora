//! Full-text search over issues, live comments and documents.
//!
//! The index is the FTS5 table `search_index` (migration 0010). Triggers on
//! `issue`, `comment` and `document` keep it current inside the transaction of
//! every change, so nothing here writes to it except [`search_rebuild`], the
//! repair that drops and repopulates it from the base tables. [`search_check`]
//! compares the numbers of indexable and indexed rows.
//!
//! # What is indexed
//!
//! * issues: title and description, plus the identifier (`TRK-12`) so that
//!   `trk 12` finds it by words; archived issues are indexed and flagged.
//! * comments: the body of live (not deleted) comments; a comment takes the
//!   archived flag of its issue.
//! * documents: title and body; archived documents are indexed and flagged.
//!
//! Archived rows are left out of results unless
//! [`SearchQuery::include_archived`] is set.
//!
//! # Queries
//!
//! The user's text never reaches FTS5 as syntax. [`parse_query`] cuts it into
//! words (whitespace separated) and `"quoted phrases"`, drops what has no
//! letter or digit, quotes every piece as an FTS5 string and joins them with
//! an implicit AND. The last word, when it is not a quoted phrase, also
//! matches as a prefix (type-ahead). At most [`MAX_QUERY_CHARS`] characters
//! and [`MAX_TOKENS`] pieces; an empty query is refused. Operators (`AND`,
//! `NEAR`, `col:`, `-`, `^`, parentheses) are plain text.
//!
//! A query of the shape `KEY-n` or `KEY n` also looks the issue up by
//! identifier; that issue is the first hit (`exact`) when the filters allow it.
//!
//! # Results
//!
//! Hits are ordered by bm25 (a title match counts 8 times a body match), ties
//! by kind then id. `total` counts matches up to [`TOTAL_CAP`] (`total_capped`
//! says the real number is higher), so paging stays cheap on common words.
//! The text of a match comes back as [`Segment`]s, never as markup: the
//! renderer escapes each segment and wraps the matched ones. Inside SQL the
//! match is bracketed by the control characters 0x01 and 0x02, which the index
//! never contains (the triggers replace them by spaces).
//!
//! # Visibility
//!
//! Version 1 has no per-project permissions: a signed-in user sees every
//! project, so [`search`] looks at all of them. When permissions arrive, this
//! function is the one place that must restrict the projects searched (the
//! `project_id` condition built in `Plan::new`); the API route and the page
//! only call it.

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params_from_iter};
use serde::Serialize;

use crate::StoreError;
use crate::projects::{get_project, invalid};

pub const DEFAULT_LIMIT: i64 = 20;
pub const MAX_LIMIT: i64 = 100;
pub const MAX_QUERY_CHARS: usize = 200;
pub const MAX_TOKENS: usize = 12;
/// Matches counted for `total`.
pub const TOTAL_CAP: i64 = 1000;
/// Characters of an issue's description shown for an identifier hit.
const EXACT_SNIPPET_CHARS: usize = 160;
/// Words of context around a match in a snippet.
const SNIPPET_WORDS: i64 = 24;

const MARK_START: char = '\u{1}';
const MARK_END: char = '\u{2}';

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Issue,
    Comment,
    Document,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Issue => "issue",
            Kind::Comment => "comment",
            Kind::Document => "document",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        match text {
            "issue" => Some(Kind::Issue),
            "comment" => Some(Kind::Comment),
            "document" => Some(Kind::Document),
            _ => None,
        }
    }

    /// A comma separated list such as `issue,comment`; at least one kind.
    pub fn parse_list(text: &str) -> Result<Vec<Kind>, StoreError> {
        let mut kinds = Vec::new();
        for part in text.split(',') {
            let part = part.trim();
            let kind = Kind::parse(part).ok_or_else(|| {
                invalid(format!(
                    "unknown kind {part:?}; use issue, comment or document"
                ))
            })?;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        Ok(kinds)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub q: String,
    /// A project key; unknown keys are `NotFound`.
    pub project: Option<String>,
    /// `None` searches every kind.
    pub kinds: Option<Vec<Kind>>,
    pub include_archived: bool,
    /// 1 to [`MAX_LIMIT`]; [`DEFAULT_LIMIT`] when `None`.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// A run of text and whether the query matched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segment {
    pub text: String,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueHit {
    pub number: i64,
    pub identifier: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentHit {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub kind: Kind,
    /// Project key.
    pub project: String,
    /// The issue itself, or the issue a comment is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentHit>,
    /// The title of the issue or document with the matches marked; empty for
    /// a comment.
    pub title: Vec<Segment>,
    /// Some words around the match in the body (the start of the body when
    /// only the title matched).
    pub snippet: Vec<Segment>,
    pub archived: bool,
    /// Found by its identifier rather than by words.
    pub exact: bool,
    /// bm25: lower is better; an exact hit has the lowest value.
    pub rank: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResults {
    /// Matches, at most [`TOTAL_CAP`].
    pub total: i64,
    pub total_capped: bool,
    pub hits: Vec<Hit>,
}

// ---- query parsing ----

/// What the user's text became.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Parsed {
    /// The FTS5 MATCH expression.
    pub fts: String,
    /// `(project key, issue number)` when the text has the `KEY-n` shape.
    pub identifier: Option<(String, i64)>,
}

fn flush(piece: &mut String, quoted: bool, pieces: &mut Vec<(String, bool)>) {
    let text = std::mem::take(piece);
    // A piece with no letter or digit has no token to match.
    if text.chars().any(char::is_alphanumeric) {
        pieces.push((text, quoted));
    }
}

pub(crate) fn parse_query(q: &str) -> Result<Parsed, StoreError> {
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(invalid(format!(
            "query is longer than {MAX_QUERY_CHARS} characters"
        )));
    }
    let mut pieces: Vec<(String, bool)> = Vec::new();
    let mut piece = String::new();
    let mut in_quote = false;
    for c in q.chars() {
        if c == '"' {
            flush(&mut piece, in_quote, &mut pieces);
            in_quote = !in_quote;
        } else if c.is_control() || (c.is_whitespace() && !in_quote) {
            if in_quote {
                piece.push(' ');
            } else {
                flush(&mut piece, false, &mut pieces);
            }
        } else {
            piece.push(c);
        }
    }
    flush(&mut piece, in_quote, &mut pieces);
    if pieces.is_empty() {
        return Err(invalid("empty query: type a word to search for"));
    }
    if pieces.len() > MAX_TOKENS {
        return Err(invalid(format!(
            "query has more than {MAX_TOKENS} words or phrases"
        )));
    }
    let last = pieces.len() - 1;
    let fts = pieces
        .iter()
        .enumerate()
        .map(|(at, (text, quoted))| {
            // Quote characters were removed above, so `text` cannot end the string.
            if at == last && !quoted {
                format!("\"{text}\" *")
            } else {
                format!("\"{text}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Parsed {
        fts,
        identifier: identifier_shape(q),
    })
}

/// `TRK-12`, `trk-12` and `trk 12` (the whole text) as `("TRK", 12)`.
fn identifier_shape(q: &str) -> Option<(String, i64)> {
    let q = q.trim();
    let (key, number) = q
        .split_once('-')
        .or_else(|| q.split_once(char::is_whitespace))?;
    let (key, number) = (key.trim(), number.trim());
    let mut chars = key.chars();
    let key_ok = (2..=10).contains(&key.len())
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric());
    let number_ok = (1..=9).contains(&number.len()) && number.bytes().all(|b| b.is_ascii_digit());
    if key_ok && number_ok {
        Some((key.to_ascii_uppercase(), number.parse().ok()?))
    } else {
        None
    }
}

/// Splits text bracketed by the match markers into segments.
pub(crate) fn segments(marked: &str) -> Vec<Segment> {
    fn push(out: &mut Vec<Segment>, text: &mut String, matched: bool) {
        if !text.is_empty() {
            out.push(Segment {
                text: std::mem::take(text),
                matched,
            });
        }
    }
    let mut out = Vec::new();
    let mut text = String::new();
    let mut matched = false;
    for c in marked.chars() {
        match c {
            MARK_START => {
                push(&mut out, &mut text, matched);
                matched = true;
            }
            MARK_END => {
                push(&mut out, &mut text, matched);
                matched = false;
            }
            other => text.push(other),
        }
    }
    push(&mut out, &mut text, matched);
    out
}

// ---- the statements ----

/// The SQL for one search and its parameters. The first parameter is the
/// MATCH expression; the main statement takes two more (limit, offset).
struct Plan {
    count_sql: String,
    main_sql: String,
    params: Vec<Value>,
    limit: i64,
    offset: i64,
    exact: Option<ExactIssue>,
}

struct ExactIssue {
    id: i64,
    project: String,
    number: i64,
    title: String,
    description: String,
    archived: bool,
}

fn check_range(what: &str, value: i64, min: i64, max: i64) -> Result<i64, StoreError> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(invalid(format!("{what} must be {min}-{max}")))
    }
}

impl Plan {
    fn new(conn: &Connection, query: &SearchQuery) -> Result<Plan, StoreError> {
        let parsed = parse_query(&query.q)?;
        let limit = check_range("limit", query.limit.unwrap_or(DEFAULT_LIMIT), 1, MAX_LIMIT)?;
        let offset = check_range("offset", query.offset.unwrap_or(0), 0, i64::from(u32::MAX))?;
        // Per-project visibility is enforced here when it exists.
        let project_id = match &query.project {
            Some(key) => Some(get_project(conn, key)?.id),
            None => None,
        };
        let kinds = match &query.kinds {
            Some(kinds) if kinds.is_empty() => {
                return Err(invalid("kind must name issue, comment or document"));
            }
            Some(kinds) => kinds.clone(),
            None => vec![Kind::Issue, Kind::Comment, Kind::Document],
        };
        let exact = match parsed.identifier {
            Some((key, number)) if kinds.contains(&Kind::Issue) => {
                find_exact(conn, &key, number, project_id, query.include_archived)?
            }
            _ => None,
        };

        let mut params = vec![Value::Text(parsed.fts)];
        let mut conds = String::from("search_index MATCH ?1");
        if let Some(id) = project_id {
            params.push(Value::Integer(id));
            conds.push_str(&format!(" AND project_id = ?{}", params.len()));
        }
        if kinds.len() < 3 {
            let list = [Kind::Issue, Kind::Comment, Kind::Document]
                .into_iter()
                .filter(|k| kinds.contains(k))
                .map(|k| format!("'{}'", k.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            conds.push_str(&format!(" AND kind IN ({list})"));
        }
        if !query.include_archived {
            conds.push_str(" AND archived = 0");
        }
        if let Some(exact) = &exact {
            // The identifier hit is added in front, so words must not repeat it.
            params.push(Value::Integer(exact.id * 4));
            conds.push_str(&format!(" AND rowid != ?{}", params.len()));
        }
        let count_sql = format!(
            "SELECT COUNT(*) FROM (SELECT 1 FROM search_index WHERE {conds} LIMIT {})",
            TOTAL_CAP + 1
        );
        let limit_at = params.len() + 1;
        // Ranking and paging touch only rowid, kind and ref_id; snippets are
        // cut afterwards for the page's rows alone, by looking each up in the
        // index by rowid (CROSS JOIN keeps `h` as the outer loop).
        let main_sql = format!(
            "SELECT s.kind, p.key, s.archived, h.score,
                    highlight(search_index, 4, char(1), char(2)),
                    snippet(search_index, 5, char(1), char(2), '\u{2026}', {SNIPPET_WORDS}),
                    i.number, i.title, c.id, d.id, d.title
             FROM (SELECT rowid AS id, kind, ref_id,
                          bm25(search_index, 0.0, 0.0, 0.0, 0.0, 8.0, 1.0, 2.0) AS score
                   FROM search_index WHERE {conds}
                   ORDER BY score, kind, ref_id
                   LIMIT ?{limit_at} OFFSET ?{}) h
             CROSS JOIN search_index s ON s.rowid = h.id AND search_index MATCH ?1
             JOIN project p ON p.id = s.project_id
             LEFT JOIN comment c ON s.kind = 'comment' AND c.id = s.ref_id
             LEFT JOIN issue i ON i.id = CASE s.kind WHEN 'issue' THEN s.ref_id
                                                     WHEN 'comment' THEN c.issue_id END
             LEFT JOIN document d ON s.kind = 'document' AND d.id = s.ref_id
             ORDER BY h.score, h.kind, h.ref_id",
            limit_at + 1
        );
        Ok(Plan {
            count_sql,
            main_sql,
            params,
            limit,
            offset,
            exact,
        })
    }
}

fn find_exact(
    conn: &Connection,
    key: &str,
    number: i64,
    project_id: Option<i64>,
    include_archived: bool,
) -> Result<Option<ExactIssue>, StoreError> {
    let found = conn
        .query_row(
            "SELECT i.id, i.project_id, p.key, i.number, i.title, i.description, i.archived
             FROM issue i JOIN project p ON p.id = i.project_id
             WHERE p.key = ?1 AND i.number = ?2",
            rusqlite::params![key, number],
            |row| {
                Ok((
                    row.get::<_, i64>(1)?,
                    ExactIssue {
                        id: row.get(0)?,
                        project: row.get(2)?,
                        number: row.get(3)?,
                        title: row.get(4)?,
                        description: row.get(5)?,
                        archived: row.get::<_, i64>(6)? != 0,
                    },
                ))
            },
        )
        .optional()?;
    Ok(found
        .filter(|(project, issue)| {
            project_id.is_none_or(|wanted| wanted == *project)
                && (include_archived || !issue.archived)
        })
        .map(|(_, issue)| issue))
}

fn exact_hit(issue: ExactIssue) -> Hit {
    let mut text: String = issue
        .description
        .chars()
        .take(EXACT_SNIPPET_CHARS)
        .collect();
    if issue.description.chars().count() > EXACT_SNIPPET_CHARS {
        text.push('\u{2026}');
    }
    let plain = |text: String| {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![Segment {
                text,
                matched: false,
            }]
        }
    };
    Hit {
        kind: Kind::Issue,
        issue: Some(IssueHit {
            number: issue.number,
            identifier: format!("{}-{}", issue.project, issue.number),
            title: issue.title.clone(),
        }),
        project: issue.project,
        comment_id: None,
        document: None,
        title: plain(issue.title),
        snippet: plain(text),
        archived: issue.archived,
        exact: true,
        rank: -1.0e9,
    }
}

fn row_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Hit>> {
    let kind: String = row.get(0)?;
    let project: String = row.get(1)?;
    let archived: i64 = row.get(2)?;
    let rank: f64 = row.get(3)?;
    let title: String = row.get(4)?;
    let snippet: String = row.get(5)?;
    let issue_number: Option<i64> = row.get(6)?;
    let issue_title: Option<String> = row.get(7)?;
    let comment_id: Option<i64> = row.get(8)?;
    let document_id: Option<i64> = row.get(9)?;
    let document_title: Option<String> = row.get(10)?;
    let Some(kind) = Kind::parse(&kind) else {
        return Ok(None);
    };
    let issue = match (issue_number, issue_title) {
        (Some(number), Some(title)) => Some(IssueHit {
            number,
            identifier: format!("{project}-{number}"),
            title,
        }),
        _ => None,
    };
    let document = match (document_id, document_title) {
        (Some(id), Some(title)) => Some(DocumentHit { id, title }),
        _ => None,
    };
    // A row whose base row is gone would be a stale index; `search_check`
    // reports that, the page simply does not show it.
    let complete = match kind {
        Kind::Issue => issue.is_some(),
        Kind::Comment => issue.is_some() && comment_id.is_some(),
        Kind::Document => document.is_some(),
    };
    if !complete {
        return Ok(None);
    }
    Ok(Some(Hit {
        kind,
        project,
        issue,
        comment_id: if kind == Kind::Comment {
            comment_id
        } else {
            None
        },
        document,
        title: segments(&title),
        snippet: segments(&snippet),
        archived: archived != 0,
        exact: false,
        rank,
    }))
}

/// Runs a search. See the module docs for the rules; errors are `Invalid`
/// (bad text or parameters) and `NotFound` (unknown project).
pub fn search(conn: &Connection, query: &SearchQuery) -> Result<SearchResults, StoreError> {
    let Plan {
        count_sql,
        main_sql,
        mut params,
        limit,
        offset,
        exact,
    } = Plan::new(conn, query)?;
    let extra = i64::from(exact.is_some());

    let counted: i64 = conn
        .query_row(&count_sql, params_from_iter(params.iter()), |row| {
            row.get(0)
        })
        .map_err(fts_error)?;
    let total = (counted + extra).min(TOTAL_CAP);
    let total_capped = counted + extra > TOTAL_CAP;

    let mut hits = Vec::new();
    if let Some(exact) = exact
        && offset == 0
    {
        hits.push(exact_hit(exact));
    }
    let take = limit - i64::try_from(hits.len()).unwrap_or(0);
    if take > 0 {
        // The identifier hit takes the first place of the first page.
        params.push(Value::Integer(take));
        params.push(Value::Integer((offset - extra).max(0)));
        let mut stmt = conn.prepare(&main_sql).map_err(fts_error)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), row_hit)
            .map_err(fts_error)?;
        for row in rows {
            if let Some(hit) = row.map_err(fts_error)? {
                hits.push(hit);
            }
        }
    }
    Ok(SearchResults {
        total,
        total_capped,
        hits,
    })
}

/// The parser builds only quoted strings, so FTS5 should never reject its
/// output; if it somehow does, that is the user's text, not a server fault.
fn fts_error(err: rusqlite::Error) -> StoreError {
    match &err {
        rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("fts5") => {
            invalid("the query could not be understood")
        }
        _ => err.into(),
    }
}

/// The query plan of the statements a search runs, for tests that show the
/// base tables are not scanned.
#[cfg(test)]
pub(crate) fn explain(conn: &Connection, query: &SearchQuery) -> Result<Vec<String>, StoreError> {
    let plan = Plan::new(conn, query)?;
    let mut params = plan.params;
    params.push(Value::Integer(plan.limit));
    params.push(Value::Integer(plan.offset));
    let mut lines = Vec::new();
    for sql in [plan.count_sql, plan.main_sql] {
        let wanted = sql.matches('?').count().min(params.len());
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map(params_from_iter(params[..wanted].iter()), |row| {
            row.get::<_, String>(3)
        })?;
        for row in rows {
            lines.push(row?);
        }
    }
    Ok(lines)
}

// ---- consistency and repair ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Counts {
    pub issues: i64,
    pub comments: i64,
    pub documents: i64,
}

/// Rows that should be indexed against rows that are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SearchCheck {
    pub indexable: Counts,
    pub indexed: Counts,
}

impl SearchCheck {
    pub fn is_consistent(&self) -> bool {
        self.indexable == self.indexed
    }
}

pub fn search_check(conn: &Connection) -> Result<SearchCheck, StoreError> {
    let count =
        |sql: &str| -> Result<i64, StoreError> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    let indexed = |kind: &str| -> Result<i64, StoreError> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM search_index WHERE kind = ?1",
            [kind],
            |r| r.get(0),
        )?)
    };
    Ok(SearchCheck {
        indexable: Counts {
            issues: count("SELECT COUNT(*) FROM issue")?,
            comments: count("SELECT COUNT(*) FROM comment WHERE deleted IS NULL")?,
            documents: count("SELECT COUNT(*) FROM document")?,
        },
        indexed: Counts {
            issues: indexed("issue")?,
            comments: indexed("comment")?,
            documents: indexed("document")?,
        },
    })
}

/// Drops the whole index and builds it again from the base tables, in the
/// caller's transaction. The same statements as migration 0010's backfill.
pub fn search_rebuild(tx: &Transaction<'_>) -> Result<SearchCheck, StoreError> {
    tx.execute("DELETE FROM search_index", [])?;
    tx.execute_batch(
        "INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
         SELECT i.id * 4, 'issue', i.project_id, i.id, i.archived,
                replace(replace(i.title, char(1), ' '), char(2), ' '),
                replace(replace(i.description, char(1), ' '), char(2), ' '),
                p.key || '-' || i.number
         FROM issue i JOIN project p ON p.id = i.project_id;

         INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
         SELECT c.id * 4 + 1, 'comment', i.project_id, c.id, i.archived, '',
                replace(replace(c.body, char(1), ' '), char(2), ' '), ''
         FROM comment c JOIN issue i ON i.id = c.issue_id
         WHERE c.deleted IS NULL;

         INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
         SELECT d.id * 4 + 2, 'document', d.project_id, d.id, d.archived,
                replace(replace(d.title, char(1), ' '), char(2), ' '),
                replace(replace(d.body, char(1), ' '), char(2), ' '), ''
         FROM document d;",
    )?;
    search_check(tx)
}
