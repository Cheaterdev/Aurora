//! What each page needs from the store, read in one go on a read-only connection.

use std::collections::HashMap;

use aurora_store::agents::{self, Agent};
use aurora_store::auth::{self, ApiKey, Principal, User};
use aurora_store::comments::{self, Comment};
use aurora_store::documents::{self, DocumentDetail, DocumentSummary, Revision, RevisionSummary};
use aurora_store::events::{self, Event};
use aurora_store::initiatives::{self, Initiative, InitiativeDetail, InitiativeView};
use aurora_store::issues::{self, Issue, IssueDetail, IssueFilter};
use aurora_store::links::{self, Blocker};
use aurora_store::projects::{self, Column, IssueType, Label, Project};
use aurora_store::prompt::{self, PromptTemplate, TemplateSource, VariableInfo};
use aurora_store::rusqlite::Connection;
use aurora_store::search::{self, Kind, SearchQuery, SearchResults};
use aurora_store::{Rejection, StoreError, now_millis};
use axum::http::StatusCode;

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

pub struct DocumentsData {
    pub user: User,
    pub project: Project,
    /// `(depth, document)` in tree order.
    pub documents: Vec<(usize, DocumentSummary)>,
    /// Preselected parent of the new-document form.
    pub parent: Option<i64>,
    pub last_event: i64,
}

pub struct DocumentData {
    pub user: User,
    pub project: Project,
    pub detail: DocumentDetail,
    pub last_event: i64,
}

pub struct HistoryData {
    pub user: User,
    pub project: Project,
    pub detail: DocumentDetail,
    /// Newest first.
    pub revisions: Vec<RevisionSummary>,
}

pub struct RevisionData {
    pub user: User,
    pub project: Project,
    pub detail: DocumentDetail,
    pub revision: Revision,
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

/// Results per page of `/search`.
pub const SEARCH_PAGE_SIZE: i64 = 20;

/// What the search page was asked for, as the address says it.
pub struct SearchRequest {
    pub q: String,
    pub project: Option<String>,
    pub kind: Option<String>,
    pub include_archived: bool,
    /// 1-based.
    pub page: i64,
}

pub enum SearchOutcome {
    /// No words were given yet.
    Idle,
    Found(SearchResults),
    Refused {
        message: String,
        status: StatusCode,
    },
}

pub struct SearchData {
    pub user: User,
    pub projects: Vec<Project>,
    pub q: String,
    pub project: Option<String>,
    pub kind: Option<Kind>,
    pub include_archived: bool,
    pub page: i64,
    pub outcome: SearchOutcome,
}

pub fn load_search(
    conn: &Connection,
    user_id: i64,
    request: SearchRequest,
) -> Result<SearchData, StoreError> {
    let SearchRequest {
        q,
        project,
        kind,
        include_archived,
        page,
    } = request;
    let kind_text = kind.filter(|k| !k.is_empty());
    let kind = kind_text.as_deref().and_then(Kind::parse);
    let refused = |message: String, status| SearchOutcome::Refused { message, status };
    let outcome = if kind_text.is_some() && kind.is_none() {
        refused(
            "Look in issues, comments or documents.".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
    } else if q.trim().is_empty() {
        SearchOutcome::Idle
    } else {
        let query = SearchQuery {
            q: q.clone(),
            project: project.clone(),
            kinds: kind.map(|k| vec![k]),
            include_archived,
            limit: Some(SEARCH_PAGE_SIZE),
            offset: Some((page - 1) * SEARCH_PAGE_SIZE),
        };
        match search::search(conn, &query) {
            Ok(found) => SearchOutcome::Found(found),
            Err(StoreError::Rejected(Rejection::NotFound(_))) => refused(
                "There is no such project.".to_string(),
                StatusCode::NOT_FOUND,
            ),
            Err(StoreError::Rejected(Rejection::Invalid(reason))) => {
                refused(reason, StatusCode::UNPROCESSABLE_ENTITY)
            }
            Err(other) => return Err(other),
        }
    };
    Ok(SearchData {
        user: auth::get_user(conn, user_id)?,
        projects: projects::list_projects(conn)?,
        q,
        project,
        kind,
        include_archived,
        page,
        outcome,
    })
}

pub fn load_home(conn: &Connection, user_id: i64) -> Result<HomeData, StoreError> {
    Ok(HomeData {
        user: auth::get_user(conn, user_id)?,
        projects: projects::list_projects(conn)?,
    })
}

pub struct AgentsData {
    pub user: User,
    pub owner: bool,
    pub agents: Vec<Agent>,
    pub projects: Vec<Project>,
}

pub struct AgentData {
    pub user: User,
    pub owner: bool,
    pub agent: Agent,
    pub projects: Vec<Project>,
    /// Only the owner may list another user's keys.
    pub keys: Vec<ApiKey>,
    /// Template text is owner-only, so these stay empty for everyone else.
    pub prompt: Option<PromptData>,
}

/// What the "Prompt" section of the agent page shows.
pub struct PromptData {
    pub default_source: TemplateSource,
    pub templates: Vec<PromptTemplate>,
    /// Issue types per project key, for the override form.
    pub issue_types: Vec<(String, Vec<IssueType>)>,
    pub variables: Vec<VariableInfo>,
}

pub fn load_agents(conn: &Connection, reader: &Principal) -> Result<AgentsData, StoreError> {
    Ok(AgentsData {
        user: auth::get_user(conn, reader.user_id)?,
        owner: reader.is_owner(),
        agents: agents::list_agents(conn, reader)?,
        projects: projects::list_projects(conn)?,
    })
}

pub fn load_agent(conn: &Connection, reader: &Principal, id: i64) -> Result<AgentData, StoreError> {
    let agent = agents::get_agent(conn, reader, id)?;
    let keys = if reader.is_owner() {
        auth::list_keys(conn, reader, agent.user_id)?
    } else {
        Vec::new()
    };
    let all_projects = projects::list_projects(conn)?;
    let prompt = if reader.is_owner() {
        let mut issue_types = Vec::new();
        for project in &all_projects {
            issue_types.push((
                project.key.clone(),
                projects::list_issue_types(conn, &project.key)?,
            ));
        }
        Some(PromptData {
            default_source: prompt::agent_default_source(conn, id)?,
            templates: prompt::list_templates(conn, reader, id)?,
            issue_types,
            variables: prompt::variable_catalogue(),
        })
    } else {
        None
    };
    Ok(AgentData {
        user: auth::get_user(conn, reader.user_id)?,
        owner: reader.is_owner(),
        agent,
        projects: all_projects,
        keys,
        prompt,
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

pub fn load_documents(
    conn: &Connection,
    user_id: i64,
    key: &str,
    parent: Option<i64>,
) -> Result<DocumentsData, StoreError> {
    let last_event = events::latest_event_id(conn)?;
    Ok(DocumentsData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        documents: documents::in_tree_order(documents::list_documents(conn, key, false)?),
        parent,
        last_event,
    })
}

pub fn load_document(
    conn: &Connection,
    user_id: i64,
    key: &str,
    id: i64,
) -> Result<DocumentData, StoreError> {
    let last_event = events::latest_event_id(conn)?;
    Ok(DocumentData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        detail: documents::get_document(conn, key, id)?,
        last_event,
    })
}

pub fn load_history(
    conn: &Connection,
    user_id: i64,
    key: &str,
    id: i64,
) -> Result<HistoryData, StoreError> {
    Ok(HistoryData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        detail: documents::get_document(conn, key, id)?,
        revisions: documents::list_revisions(conn, key, id)?,
    })
}

pub fn load_revision(
    conn: &Connection,
    user_id: i64,
    key: &str,
    id: i64,
    number: i64,
) -> Result<RevisionData, StoreError> {
    Ok(RevisionData {
        user: auth::get_user(conn, user_id)?,
        project: projects::get_project(conn, key)?,
        detail: documents::get_document(conn, key, id)?,
        revision: documents::get_revision(conn, key, id, number)?,
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
