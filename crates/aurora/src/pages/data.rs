//! What each page needs from the store, read in one go on a read-only connection.

use std::collections::HashMap;

use aurora_store::auth::{self, User};
use aurora_store::comments::{self, Comment};
use aurora_store::events::{self, Event};
use aurora_store::initiatives::{self, Initiative, InitiativeDetail, InitiativeView};
use aurora_store::issues::{self, Issue, IssueDetail, IssueFilter};
use aurora_store::links::{self, Blocker};
use aurora_store::projects::{self, Column, IssueType, Label, Project};
use aurora_store::rusqlite::Connection;
use aurora_store::{StoreError, now_millis};

/// How many log entries the issue page shows.
const ACTIVITY_LIMIT: i64 = 100;

pub struct HomeData {
    pub user: User,
    pub projects: Vec<Project>,
}

pub struct BoardData {
    pub user: User,
    pub project: Project,
    pub columns: Vec<Column>,
    pub issues: Vec<Issue>,
    pub types: Vec<IssueType>,
    pub labels: Vec<Label>,
    pub users: Vec<User>,
    pub initiatives: Vec<Initiative>,
    /// Open blockers of the blocked issues, by issue id.
    pub blockers: HashMap<i64, Vec<Blocker>>,
    /// Read before the rest, so a stream started after it misses nothing.
    pub last_event: i64,
}

pub struct IssueData {
    pub user: User,
    pub project: Project,
    pub detail: IssueDetail,
    pub parent_title: Option<String>,
    pub columns: Vec<Column>,
    pub types: Vec<IssueType>,
    pub labels: Vec<Label>,
    pub users: Vec<User>,
    pub initiatives: Vec<Initiative>,
    pub activity: Vec<Event>,
    pub comments: Vec<Comment>,
    pub last_event: i64,
}

pub struct InitiativesData {
    pub user: User,
    pub project: Project,
    pub initiatives: Vec<InitiativeView>,
    pub last_event: i64,
}

pub struct InitiativeData {
    pub user: User,
    pub project: Project,
    pub detail: InitiativeDetail,
    pub columns: Vec<Column>,
    pub last_event: i64,
}

/// The project's initiatives without their progress, to name them.
fn project_initiatives(conn: &Connection, key: &str) -> Result<Vec<Initiative>, StoreError> {
    Ok(
        initiatives::list_initiatives(conn, now_millis(), key, None)?
            .into_iter()
            .map(|view| view.initiative)
            .collect(),
    )
}

pub fn load_home(conn: &Connection, user_id: i64) -> Result<HomeData, StoreError> {
    Ok(HomeData {
        user: auth::get_user(conn, user_id)?,
        projects: projects::list_projects(conn)?,
    })
}

/// Every open issue of the project (the API pages at 200).
fn all_issues(conn: &Connection, key: &str) -> Result<Vec<Issue>, StoreError> {
    let mut found = Vec::new();
    loop {
        let filter = IssueFilter {
            limit: Some(issues::MAX_LIMIT),
            offset: Some(i64::try_from(found.len()).unwrap_or(i64::MAX)),
            ..IssueFilter::default()
        };
        let page = issues::list_issues(conn, key, &filter)?;
        let got = page.issues.len();
        found.extend(page.issues);
        if got == 0 || i64::try_from(found.len()).unwrap_or(i64::MAX) >= page.total {
            return Ok(found);
        }
    }
}

pub fn load_board(conn: &Connection, user_id: i64, key: &str) -> Result<BoardData, StoreError> {
    let last_event = events::latest_event_id(conn)?;
    Ok(BoardData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        columns: projects::list_columns(conn, key)?,
        issues: all_issues(conn, key)?,
        types: projects::list_issue_types(conn, key)?,
        labels: projects::list_labels(conn, key)?,
        users: auth::list_users(conn)?,
        initiatives: project_initiatives(conn, key)?,
        blockers: links::open_blockers(conn, key)?,
        last_event,
    })
}

pub fn load_initiatives(
    conn: &Connection,
    user_id: i64,
    key: &str,
) -> Result<InitiativesData, StoreError> {
    let last_event = events::latest_event_id(conn)?;
    Ok(InitiativesData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        initiatives: initiatives::list_initiatives(conn, now_millis(), key, None)?,
        last_event,
    })
}

pub fn load_initiative(
    conn: &Connection,
    user_id: i64,
    key: &str,
    id: i64,
) -> Result<InitiativeData, StoreError> {
    let last_event = events::latest_event_id(conn)?;
    Ok(InitiativeData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        detail: initiatives::get_initiative(
            conn,
            now_millis(),
            key,
            id,
            Some(issues::MAX_LIMIT),
            None,
        )?,
        columns: projects::list_columns(conn, key)?,
        last_event,
    })
}

pub fn load_issue(
    conn: &Connection,
    user_id: i64,
    key: &str,
    number: i64,
) -> Result<IssueData, StoreError> {
    let last_event = events::latest_event_id(conn)?;
    let detail = issues::get_issue(conn, key, number)?;
    let parent_title = match detail.issue.parent {
        Some(parent) => Some(issues::get_issue(conn, key, parent)?.issue.title),
        None => None,
    };
    let activity = events::entity_events(conn, "issue", detail.issue.id, ACTIVITY_LIMIT)?;
    Ok(IssueData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        detail,
        parent_title,
        columns: projects::list_columns(conn, key)?,
        types: projects::list_issue_types(conn, key)?,
        labels: projects::list_labels(conn, key)?,
        users: auth::list_users(conn)?,
        initiatives: project_initiatives(conn, key)?,
        activity,
        comments: comments::list_comments(conn, key, number)?,
        last_event,
    })
}

/// The comments list alone, for the live refresh of an issue page.
pub struct CommentsData {
    pub viewer: User,
    pub comments: Vec<Comment>,
}

pub fn load_comments(
    conn: &Connection,
    user_id: i64,
    key: &str,
    number: i64,
) -> Result<CommentsData, StoreError> {
    Ok(CommentsData {
        viewer: auth::get_user(conn, user_id)?,
        comments: comments::list_comments(conn, key, number)?,
    })
}

/// The activity list alone, for the live refresh of an issue page.
pub struct ActivityData {
    pub columns: Vec<Column>,
    pub types: Vec<IssueType>,
    pub labels: Vec<Label>,
    pub users: Vec<User>,
    pub initiatives: Vec<Initiative>,
    pub activity: Vec<Event>,
}

pub fn load_activity(
    conn: &Connection,
    key: &str,
    number: i64,
) -> Result<ActivityData, StoreError> {
    let detail = issues::get_issue(conn, key, number)?;
    Ok(ActivityData {
        columns: projects::list_columns(conn, key)?,
        types: projects::list_issue_types(conn, key)?,
        labels: projects::list_labels(conn, key)?,
        users: auth::list_users(conn)?,
        initiatives: project_initiatives(conn, key)?,
        activity: events::entity_events(conn, "issue", detail.issue.id, ACTIVITY_LIMIT)?,
    })
}
