//! The HTML of every page, as `maud` templates. `maud` escapes every
//! interpolated value, so user content is never raw HTML; nothing here uses
//! `PreEscaped`. The one exception is a document body, which goes through
//! `crate::markdown::render` and is interpolated as the `Markup` it returns:
//! that renderer is the only producer of unescaped user-derived HTML. Data for
//! the script goes in `data-*` attributes: the content security policy
//! forbids inline script and style.

use std::collections::HashMap;

use aurora_store::agents::Agent;
use aurora_store::auth::{Role, User};
use aurora_store::comments::{Comment, mention_ranges};
use aurora_store::documents::{DocumentDetail, DocumentLinks, DocumentSummary, LinkedDocument};
use aurora_store::events::Event;
use aurora_store::initiatives::{Health, Initiative, InitiativeStatus, InitiativeView, Progress};
use aurora_store::issues::{Issue, Priority};
use aurora_store::links::{Blocker, IssueLinks, LinkedIssue};
use aurora_store::projects::{Column, IssueType, Label, Project};
use aurora_store::prompt::{TemplateSource, VariableInfo};
use aurora_store::runs::{Run, RunStatus, WorktreeState};
use aurora_store::search::{Hit, Kind, Segment};
use maud::{DOCTYPE, Markup, html};
use serde_json::Value;

use super::assets;
use super::data::{
    ActivityData, AgentData, AgentsData, BoardData, CommentsData, DocumentData, DocumentsData,
    HistoryData, HomeData, InitiativeData, InitiativesData, IssueData, PromptData, RevisionData,
    RunPageData, RunsData, SEARCH_PAGE_SIZE, SearchData, SearchOutcome,
};
use super::time::{utc_iso, utc_text};
use crate::markdown;

const PRIORITIES: [Priority; 4] = [
    Priority::Urgent,
    Priority::High,
    Priority::Medium,
    Priority::Low,
];

const STATUSES: [InitiativeStatus; 5] = [
    InitiativeStatus::Planned,
    InitiativeStatus::Active,
    InitiativeStatus::Paused,
    InitiativeStatus::Completed,
    InitiativeStatus::Canceled,
];

pub struct Frame<'a> {
    pub title: &'a str,
    /// `board`, `issue`, `run`, `initiatives`, `initiative`, `documents`, `document`, `history`,
    /// `revision`, `home`, `sign-in` or `setup`; the script picks its behaviour from it.
    pub page: &'static str,
    pub user: Option<&'a User>,
    pub project: Option<&'a Project>,
    pub last_event: Option<i64>,
    pub issue_id: Option<i64>,
    pub issue_number: Option<i64>,
    pub initiative_id: Option<i64>,
    pub document_id: Option<i64>,
    pub run_id: Option<i64>,
    /// The text the top bar's search box starts with.
    pub search: Option<&'a str>,
}

impl<'a> Frame<'a> {
    pub fn new(title: &'a str, page: &'static str) -> Self {
        Frame {
            title,
            page,
            user: None,
            project: None,
            last_event: None,
            issue_id: None,
            issue_number: None,
            initiative_id: None,
            document_id: None,
            run_id: None,
            search: None,
        }
    }
}

fn layout(frame: &Frame<'_>, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (frame.title) " \u{2013} Aurora" }
                link rel="icon" type="image/svg+xml" href=(assets::icon_url());
                link rel="stylesheet" href=(assets::css_url());
                script src=(assets::js_url()) defer {}
            }
            body data-page=(frame.page)
                data-project=[frame.project.map(|p| p.key.as_str())]
                data-last-event=[frame.last_event]
                data-issue-id=[frame.issue_id]
                data-issue-number=[frame.issue_number]
                data-initiative-id=[frame.initiative_id]
                data-document-id=[frame.document_id]
                data-run-id=[frame.run_id] {
                a.skip href="#main" { "Skip to content" }
                header.topbar {
                    a.brand href="/" { "Aurora" }
                    @if frame.user.is_some() {
                        nav.main-nav aria-label="Main" {
                            a href="/agents" { "Agents" }
                        }
                    }
                    @if let Some(project) = frame.project {
                        nav.project-nav aria-label="Project" {
                            a.project-link href={ "/p/" (project.key) } {
                                span.key { (project.key) }
                                " "
                                (project.name)
                            }
                            a href={ "/p/" (project.key) "/initiatives" } { "Initiatives" }
                            a href={ "/p/" (project.key) "/documents" } { "Documents" }
                            a href="/?list=1" { "All projects" }
                        }
                    }
                    @if frame.user.is_some() {
                        form.search method="get" action="/search" role="search" {
                            label.sr-only for="search-q" { "Search issues, comments and documents" }
                            input #search-q type="search" name="q" maxlength="200"
                                value=[frame.search] placeholder="Search (press /)"
                                autocomplete="off" aria-keyshortcuts="/";
                            button type="submit" { "Search" }
                        }
                    }
                    @if let Some(user) = frame.user {
                        details.user-menu {
                            summary { (user.name) }
                            div.menu {
                                button type="button" data-action="sign-out" { "Sign out" }
                            }
                        }
                    }
                }
                main #main { (content) }
                div #toasts.toasts role="status" aria-live="polite" {}
            }
        }
    }
}

// ---- sign-in, setup, errors ----

pub fn sign_in(next: Option<&str>, error: Option<&str>) -> Markup {
    let frame = Frame::new("Sign in", "sign-in");
    layout(
        &frame,
        html! {
            section.card-form {
                h1 { "Sign in" }
                @if let Some(error) = error {
                    p.form-error role="alert" { (error) }
                }
                form method="post" action="/sign-in" {
                    div.field {
                        label for="name" { "Name" }
                        input #name name="name" autocomplete="username" required autofocus;
                    }
                    div.field {
                        label for="password" { "Password" }
                        input #password name="password" type="password"
                            autocomplete="current-password" required;
                    }
                    @if let Some(next) = next {
                        input type="hidden" name="next" value=(next);
                    }
                    button.primary type="submit" { "Sign in" }
                }
            }
        },
    )
}

pub fn setup(error: Option<&str>) -> Markup {
    let frame = Frame::new("Set up", "setup");
    layout(
        &frame,
        html! {
            section.card-form {
                h1 { "Welcome to Aurora" }
                p { "Create the owner account. You will be signed in afterwards." }
                @if let Some(error) = error {
                    p.form-error role="alert" { (error) }
                }
                form method="post" action="/setup" {
                    div.field {
                        label for="name" { "Name" }
                        input #name name="name" autocomplete="username" required autofocus;
                    }
                    div.field {
                        label for="password" { "Password" }
                        input #password name="password" type="password"
                            autocomplete="new-password" required;
                    }
                    button.primary type="submit" { "Create owner" }
                }
            }
        },
    )
}

pub fn message(title: &str, text: &str, user: Option<&User>) -> Markup {
    let mut frame = Frame::new(title, "message");
    frame.user = user;
    layout(
        &frame,
        html! {
            section.card-form {
                h1 { (title) }
                p { (text) }
                p { a href="/" { "Back to the projects" } }
            }
        },
    )
}

// ---- search ----

/// Matched runs in `<mark>`; every piece of text is escaped by maud.
fn marked(segments: &[Segment]) -> Markup {
    html! {
        @for segment in segments {
            @if segment.matched { mark { (segment.text) } } @else { (segment.text) }
        }
    }
}

fn search_url(data: &SearchData, page: i64) -> String {
    let mut url = format!("/search?q={}", super::encode_component(&data.q));
    if let Some(project) = &data.project {
        url.push_str(&format!("&project={}", super::encode_component(project)));
    }
    if let Some(kind) = data.kind {
        url.push_str(&format!("&kind={}", kind.as_str()));
    }
    if data.include_archived {
        url.push_str("&include_archived=1");
    }
    if page > 1 {
        url.push_str(&format!("&page={page}"));
    }
    url
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Issue => "Issue",
        Kind::Comment => "Comment",
        Kind::Document => "Document",
    }
}

fn search_hit(hit: &Hit) -> Markup {
    let key = hit.project.as_str();
    let href = match (hit.kind, &hit.issue, &hit.document) {
        (Kind::Issue, Some(issue), _) => format!("/p/{key}/{}", issue.number),
        (Kind::Comment, Some(issue), _) => format!(
            "/p/{key}/{}#comment-{}",
            issue.number,
            hit.comment_id.unwrap_or_default()
        ),
        (_, _, Some(document)) => format!("/p/{key}/documents/{}", document.id),
        _ => format!("/p/{key}"),
    };
    html! {
        li.search-hit {
            div.hit-head {
                span.chip.kind-badge { (kind_label(hit.kind)) }
                a.hit-title href=(href) {
                    @if let Some(issue) = &hit.issue {
                        span.key { (issue.identifier) }
                        @if hit.kind == Kind::Issue && !hit.title.is_empty() {
                            (marked(&hit.title))
                        } @else {
                            (issue.title)
                        }
                    } @else if let Some(document) = &hit.document {
                        @if hit.title.is_empty() { (document.title) } @else { (marked(&hit.title)) }
                    }
                }
                @if hit.archived { span.chip.chip-archived { "Archived" } }
                @if hit.exact { span.chip { "Identifier match" } }
            }
            @if !hit.snippet.is_empty() {
                p.hit-snippet { (marked(&hit.snippet)) }
            }
        }
    }
}

pub fn search_page(data: &SearchData) -> Markup {
    let mut frame = Frame::new("Search", "search");
    frame.user = Some(&data.user);
    frame.search = Some(&data.q);
    let kinds = [
        (None, "Everything"),
        (Some(Kind::Issue), "Issues"),
        (Some(Kind::Comment), "Comments"),
        (Some(Kind::Document), "Documents"),
    ];
    layout(
        &frame,
        html! {
            section.page.search-page {
                h1 { "Search" }
                form.search-filters method="get" action="/search" {
                    div.field {
                        label for="search-page-q" { "Words" }
                        input #search-page-q type="search" name="q" value=(data.q)
                            maxlength="200" autocomplete="off";
                    }
                    div.field {
                        label for="search-project" { "Project" }
                        select #search-project name="project" {
                            option value="" selected[data.project.is_none()] { "All projects" }
                            @for project in &data.projects {
                                option value=(project.key)
                                    selected[data.project.as_deref() == Some(project.key.as_str())] {
                                    (project.key) " " (project.name)
                                }
                            }
                        }
                    }
                    div.field {
                        label for="search-kind" { "Look in" }
                        select #search-kind name="kind" {
                            @for (kind, label) in kinds {
                                option value=(kind.map_or("", Kind::as_str)) selected[data.kind == kind] {
                                    (label)
                                }
                            }
                        }
                    }
                    div.field.check {
                        input #search-archived type="checkbox" name="include_archived" value="1"
                            checked[data.include_archived];
                        label for="search-archived" { "Include archived" }
                    }
                    button.primary type="submit" { "Search" }
                }
                @match &data.outcome {
                    SearchOutcome::Idle => {
                        p.empty {
                            "Type words to find issues, comments and documents. "
                            "An identifier such as TRK-12 jumps to that issue."
                        }
                    }
                    SearchOutcome::Refused { message, .. } => {
                        p.form-error role="alert" { (message) }
                    }
                    SearchOutcome::Found(results) if results.hits.is_empty() => {
                        p.empty role="status" {
                            "Nothing found for \u{201c}" (data.q) "\u{201d}. "
                            "Try fewer or different words, or loosen the filters."
                        }
                    }
                    SearchOutcome::Found(results) => {
                        @let first = (data.page - 1) * SEARCH_PAGE_SIZE + 1;
                        @let last = first + i64::try_from(results.hits.len()).unwrap_or(0) - 1;
                        p.search-summary role="status" {
                            "Results " (first) "\u{2013}" (last) " of "
                            (results.total) @if results.total_capped { "+" }
                        }
                        ol.search-results {
                            @for hit in &results.hits { (search_hit(hit)) }
                        }
                        nav.pager aria-label="Result pages" {
                            @if data.page > 1 {
                                a.button href=(search_url(data, data.page - 1)) { "Previous" }
                            }
                            span.pager-page { "Page " (data.page) }
                            @if last < results.total {
                                a.button href=(search_url(data, data.page + 1)) { "Next" }
                            }
                        }
                    }
                }
            }
        },
    )
}

// ---- home ----

pub fn home(data: &HomeData) -> Markup {
    let mut frame = Frame::new("Projects", "home");
    frame.user = Some(&data.user);
    layout(
        &frame,
        html! {
            section.page {
                h1 { "Projects" }
                @if data.projects.is_empty() {
                    p.empty { "No projects yet. Create the first one below." }
                } @else {
                    ul.project-list {
                        @for project in &data.projects {
                            li {
                                a href={ "/p/" (project.key) } {
                                    span.key { (project.key) }
                                    " "
                                    (project.name)
                                }
                            }
                        }
                    }
                }
                h2 { "New project" }
                form.inline-form data-form="new-project" {
                    div.field {
                        label for="project-key" { "Key" }
                        input #project-key name="key" required minlength="2" maxlength="10"
                            pattern="[A-Z][A-Z0-9]{1,9}" placeholder="TRK" autocomplete="off"
                            title="2-10 characters: an uppercase letter, then uppercase letters or digits";
                    }
                    div.field {
                        label for="project-name" { "Name" }
                        input #project-name name="name" required maxlength="100"
                            placeholder="Tracker" autocomplete="off";
                    }
                    button.primary type="submit" { "Create project" }
                    p.field-msg role="alert" {}
                }
            }
        },
    )
}

// ---- lookups shared by the board and the issue page ----

struct Lookup<'a> {
    columns: HashMap<i64, &'a str>,
    types: HashMap<i64, &'a str>,
    labels: HashMap<i64, &'a Label>,
    users: HashMap<i64, &'a str>,
    initiatives: HashMap<i64, &'a str>,
}

impl<'a> Lookup<'a> {
    fn new(
        columns: &'a [Column],
        types: &'a [IssueType],
        labels: &'a [Label],
        users: &'a [User],
        initiatives: &'a [Initiative],
    ) -> Self {
        Lookup {
            columns: columns.iter().map(|c| (c.id, c.name.as_str())).collect(),
            types: types.iter().map(|t| (t.id, t.name.as_str())).collect(),
            labels: labels.iter().map(|l| (l.id, l)).collect(),
            users: users.iter().map(|u| (u.id, u.name.as_str())).collect(),
            initiatives: initiatives
                .iter()
                .map(|i| (i.id, i.name.as_str()))
                .collect(),
        }
    }

    fn initiative(&self, id: i64) -> String {
        self.initiatives
            .get(&id)
            .map_or_else(|| format!("initiative {id}"), |n| (*n).to_string())
    }

    fn column(&self, id: i64) -> String {
        self.columns
            .get(&id)
            .map_or_else(|| format!("column {id}"), |n| (*n).to_string())
    }

    fn type_name(&self, id: i64) -> String {
        self.types
            .get(&id)
            .map_or_else(|| format!("type {id}"), |n| (*n).to_string())
    }

    fn user(&self, id: i64) -> String {
        self.users
            .get(&id)
            .map_or_else(|| format!("user {id}"), |n| (*n).to_string())
    }

    fn label(&self, id: i64) -> String {
        self.labels
            .get(&id)
            .map_or_else(|| format!("label {id}"), |l| l.name.clone())
    }
}

// ---- board ----

fn add_form(column: &Column, types: &[IssueType]) -> Markup {
    html! {
        div.add {
            @if types.is_empty() {
                p.hint { "Add an issue type under Project tools to create issues." }
            } @else {
                button.add-btn type="button" data-action="show-add" { "+ Add issue" }
                form.add-form data-form="add-issue" data-column-id=(column.id) hidden {
                    label.sr-only for={ "title-" (column.id) } { "Title of the new issue" }
                    input id={ "title-" (column.id) } name="title" required maxlength="300"
                        placeholder="Issue title" autocomplete="off";
                    @if types.len() > 1 {
                        label.sr-only for={ "type-" (column.id) } { "Type" }
                        select id={ "type-" (column.id) } name="type_id" {
                            @for kind in types {
                                option value=(kind.id) { (kind.name) }
                            }
                        }
                    }
                    div.row {
                        button.primary type="submit" { "Add" }
                        button type="button" data-action="hide-add" { "Cancel" }
                    }
                    p.field-msg role="alert" {}
                }
            }
        }
    }
}

fn blocker_names(blockers: &[Blocker]) -> String {
    blockers
        .iter()
        .map(|b| b.identifier.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn card(issue: &Issue, key: &str, data: &BoardData, lookup: &Lookup<'_>) -> Markup {
    let blockers = data
        .blockers
        .get(&issue.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    html! {
        li.card.blocked[issue.blocked] draggable="true" data-number=(issue.number)
            data-column-id=(issue.column_id) {
            a.card-link href={ "/p/" (key) "/" (issue.number) } draggable="false" {
                span.ident { (issue.identifier) }
                " "
                span.card-title { (issue.title) }
            }
            div.card-meta {
                @if issue.blocked {
                    span.chip.chip-blocked title={ "Blocked by " (blocker_names(blockers)) } {
                        "Blocked"
                        span.sr-only { " by " (blocker_names(blockers)) }
                    }
                }
                span.chip.chip-type { (lookup.type_name(issue.type_id)) }
                @if let Some(priority) = issue.priority {
                    span.chip.chip-priority data-priority=(priority.as_str()) {
                        (priority.as_str())
                    }
                }
                @for id in &issue.labels {
                    @if let Some(label) = lookup.labels.get(id) {
                        span.chip.chip-label data-color=(label.color) { (label.name) }
                    }
                }
                @if let Some(assignee) = issue.assignee_id {
                    span.chip.chip-assignee { (lookup.user(assignee)) }
                }
                @if let Some(initiative) = issue.initiative_id {
                    span.chip.chip-initiative {
                        span.sr-only { "Initiative: " }
                        (lookup.initiative(initiative))
                    }
                }
            }
            label.move-to {
                span.sr-only { "Move " (issue.identifier) " to column" }
                select.move-select data-number=(issue.number) {
                    @for column in &data.columns {
                        option value=(column.id) selected[column.id == issue.column_id] {
                            (column.name)
                        }
                    }
                }
            }
        }
    }
}

/// The board itself; also what `GET /p/{key}/board` returns for live refreshes.
pub fn board_fragment(data: &BoardData) -> Markup {
    let lookup = Lookup::new(
        &data.columns,
        &data.types,
        &data.labels,
        &data.users,
        &data.initiatives,
    );
    let key = data.project.key.as_str();
    html! {
        div #board.board data-project=(key) {
            @for column in &data.columns {
                @let cards: Vec<&Issue> =
                    data.issues.iter().filter(|i| i.column_id == column.id).collect();
                section.column data-column-id=(column.id) data-state=(column.state_type.as_str())
                    aria-labelledby={ "column-" (column.id) } {
                    header.column-head {
                        h2 id={ "column-" (column.id) } { (column.name) }
                        span.state { (column.state_type.as_str()) }
                        span.count { (cards.len()) }
                    }
                    ol.cards data-column-id=(column.id) {
                        @for issue in cards {
                            (card(issue, key, data, &lookup))
                        }
                    }
                    (add_form(column, &data.types))
                }
            }
        }
    }
}

/// The repository runs take their working copies from. Owner only: the caller
/// does not build it for anyone else.
fn repository_form(project: &Project) -> Markup {
    html! {
        form.inline-form data-form="repo-settings" {
            h3 { "Repository" }
            p.hint {
                "Each run of an agent without a fixed working folder gets its own git worktree of this repository. "
                "Leave it empty to give runs an empty folder instead."
            }
            div.field {
                label for="repo-path" { "Repository folder" }
                input #repo-path name="repo_path" maxlength="1000" autocomplete="off"
                    value=(project.repo_path.as_deref().unwrap_or(""))
                    placeholder="The folder of an existing git repository";
            }
            div.field {
                label for="base-branch" { "Base branch" }
                input #base-branch name="base_branch" maxlength="200" autocomplete="off"
                    value=(project.base_branch.as_deref().unwrap_or(""))
                    placeholder="Empty: the repository's default branch";
            }
            button type="submit" { "Save repository" }
            p.field-msg role="alert" {}
        }
    }
}

pub fn board_page(data: &BoardData) -> Markup {
    let mut frame = Frame::new(&data.project.name, "board");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    layout(
        &frame,
        html! {
            section.board-page {
                div.board-head {
                    h1 { (data.project.name) }
                }
                @if data.types.is_empty() {
                    p.notice {
                        "This project has no issue types yet. Add one under Project tools below."
                    }
                }
                (board_fragment(data))
                details.tools {
                    summary { "Project tools" }
                    div.tools-body {
                        form.inline-form data-form="new-type" {
                            h3 { "New issue type" }
                            div.field {
                                label for="type-name" { "Name" }
                                input #type-name name="name" required maxlength="100"
                                    placeholder="Task" autocomplete="off";
                            }
                            button type="submit" { "Add type" }
                            p.field-msg role="alert" {}
                        }
                        form.inline-form data-form="new-label" {
                            h3 { "New label" }
                            div.field {
                                label for="label-name" { "Name" }
                                input #label-name name="name" required maxlength="100"
                                    placeholder="bug" autocomplete="off";
                            }
                            div.field {
                                label for="label-color" { "Color" }
                                input #label-color name="color" type="color" value="#808080";
                            }
                            button type="submit" { "Add label" }
                            p.field-msg role="alert" {}
                        }
                        @if data.user.role == Role::Owner {
                            (repository_form(&data.project))
                        }
                    }
                }
            }
        },
    )
}

// ---- initiatives ----

fn health_key(health: Health) -> &'static str {
    match health {
        Health::OnTrack => "on_track",
        Health::AtRisk => "at_risk",
        Health::OffTrack => "off_track",
        Health::None => "none",
    }
}

/// Words, so health never relies on colour alone.
fn health_text(health: Health) -> &'static str {
    match health {
        Health::OnTrack => "On track",
        Health::AtRisk => "At risk",
        Health::OffTrack => "Off track",
        Health::None => "No health signal",
    }
}

fn progress_bar(name: &str, progress: &Progress, health: Health) -> Markup {
    html! {
        div.progress-row {
            progress.progress-bar max=(progress.total.max(1)) value=(progress.done)
                aria-label={ "Progress of " (name) } {
                (progress.percent) "%"
            }
            span.progress-text {
                (progress.done) " of " (progress.total) " done (" (progress.percent) "%)"
            }
            span.chip.chip-health data-health=(health_key(health)) {
                "Health: " (health_text(health))
            }
        }
    }
}

fn initiative_item(view: &InitiativeView, key: &str) -> Markup {
    let initiative = &view.initiative;
    html! {
        li.initiative-item {
            div.initiative-head {
                a.initiative-name href={ "/p/" (key) "/initiatives/" (initiative.id) } {
                    (initiative.name)
                }
                span.chip.chip-status data-status=(initiative.status.as_str()) {
                    (initiative.status.as_str())
                }
                span.meta {
                    @if let Some(date) = &initiative.target_date {
                        "Target " (date)
                    } @else {
                        "No target date"
                    }
                }
            }
            (progress_bar(&initiative.name, &view.progress, view.health))
        }
    }
}

fn initiative_list(data: &InitiativesData) -> Markup {
    let key = data.project.key.as_str();
    html! {
        section #initiative-list aria-label="Initiatives" {
            @if data.initiatives.is_empty() {
                p.empty { "No initiatives yet. Create the first one below." }
            } @else {
                ul.initiative-list {
                    @for view in &data.initiatives { (initiative_item(view, key)) }
                }
            }
        }
    }
}

/// The list alone; also what `GET /p/{key}/initiatives/list` returns for live refreshes.
pub fn initiatives_fragment(data: &InitiativesData) -> Markup {
    initiative_list(data)
}

pub fn initiatives_page(data: &InitiativesData) -> Markup {
    let key = data.project.key.as_str();
    let mut frame = Frame::new("Initiatives", "initiatives");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    layout(
        &frame,
        html! {
            section.page {
                nav.crumbs aria-label="Breadcrumb" {
                    a href={ "/p/" (key) } { (data.project.name) }
                    " / "
                    span { "Initiatives" }
                }
                h1 { "Initiatives" }
                (initiative_list(data))
                h2 { "New initiative" }
                form.inline-form data-form="new-initiative" {
                    div.field {
                        label for="initiative-name" { "Name" }
                        input #initiative-name name="name" required maxlength="200"
                            placeholder="Launch" autocomplete="off";
                    }
                    div.field {
                        label for="initiative-target" { "Target date (optional)" }
                        input #initiative-target name="target_date" type="date";
                    }
                    button.primary type="submit" { "Create initiative" }
                    p.field-msg role="alert" {}
                }
            }
        },
    )
}

/// Progress and the issue list of one initiative; also what
/// `GET /p/{key}/initiatives/{id}/progress` returns for live refreshes.
pub fn initiative_progress_fragment(data: &InitiativeData) -> Markup {
    let key = data.project.key.as_str();
    let view = &data.detail.view;
    let progress = &view.progress;
    let columns: HashMap<i64, &str> = data
        .columns
        .iter()
        .map(|c| (c.id, c.name.as_str()))
        .collect();
    let shown = i64::try_from(data.detail.issues.len()).unwrap_or(i64::MAX);
    html! {
        section #initiative-progress aria-labelledby="progress-heading" {
            h2 #progress-heading { "Progress" }
            (progress_bar(&view.initiative.name, progress, view.health))
            p.meta {
                "Completed " (progress.completed)
                ", canceled " (progress.canceled)
                ", in progress " (progress.started)
                ", not started " (progress.open)
                ". Archived issues are not counted."
            }
            h2 #issues-heading { "Issues" }
            @if data.detail.issues.is_empty() {
                p.empty { "No issues yet. Pick this initiative on an issue's page to add it." }
            } @else {
                ul.links-list {
                    @for issue in &data.detail.issues {
                        li {
                            a href={ "/p/" (key) "/" (issue.number) } {
                                span.ident { (issue.identifier) }
                                " "
                                (issue.title)
                            }
                            " "
                            span.chip data-state=(issue.state_type.as_str()) {
                                (columns.get(&issue.column_id).copied().unwrap_or("column"))
                            }
                        }
                    }
                }
            }
            @if data.detail.total > shown {
                p.hint { "Showing the first " (shown) " of " (data.detail.total) " issues." }
            }
        }
    }
}

pub fn initiative_page(data: &InitiativeData) -> Markup {
    let initiative = &data.detail.view.initiative;
    let key = data.project.key.as_str();
    let mut frame = Frame::new(&initiative.name, "initiative");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    frame.initiative_id = Some(initiative.id);
    layout(
        &frame,
        html! {
            article.issue {
                nav.crumbs aria-label="Breadcrumb" {
                    a href={ "/p/" (key) } { (data.project.name) }
                    " / "
                    a href={ "/p/" (key) "/initiatives" } { "Initiatives" }
                    " / "
                    span { (initiative.name) }
                }
                div.issue-head {
                    h1 #initiative-title { (initiative.name) }
                    button type="button" data-action="delete-initiative" { "Delete" }
                }
                p.field-msg #page-msg role="alert" {}
                div.issue-grid {
                    div.issue-main {
                        div.field {
                            label for="f-name" { "Name" }
                            input #f-name data-field="name" value=(initiative.name) maxlength="200";
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-description" { "Description" }
                            textarea #f-description data-field="description" rows="8" {
                                "\n" (initiative.description)
                            }
                            div.row {
                                button type="button" data-action="save-description" { "Save description" }
                            }
                            p.field-msg role="alert" {}
                        }
                        (initiative_progress_fragment(data))
                        (documents_section(&data.detail.documents, key))
                    }
                    aside.issue-side aria-label="Fields" {
                        div.field {
                            label for="f-status" { "Status" }
                            select #f-status data-field="status" {
                                @for status in STATUSES {
                                    option value=(status.as_str())
                                        selected[status == initiative.status] {
                                        (status.as_str())
                                    }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-target" { "Target date" }
                            input #f-target data-field="target_date" type="date"
                                value=[initiative.target_date.as_deref()];
                            p.field-msg role="alert" {}
                        }
                        p.meta {
                            "Created "
                            time.when datetime=(utc_iso(initiative.created)) data-ts=(initiative.created) {
                                (utc_text(initiative.created))
                            }
                            "."
                        }
                    }
                }
            }
        },
    )
}

// ---- documents ----

/// The documents linked to an issue or an initiative.
fn documents_section(documents: &[LinkedDocument], key: &str) -> Markup {
    html! {
        section #documents aria-labelledby="documents-heading" {
            h2 #documents-heading { "Documents" }
            @if documents.is_empty() {
                p.empty { "No linked documents. Link one from the document's page." }
            } @else {
                ul.links-list {
                    @for doc in documents {
                        li {
                            a href={ "/p/" (key) "/documents/" (doc.id) } { (doc.title) }
                            " "
                            button type="button" data-action="remove-doc-link"
                                data-document-id=(doc.id) data-link-id=(doc.link_id) {
                                "Unlink"
                                span.sr-only { " " (doc.title) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn document_options(documents: &[(usize, DocumentSummary)], selected: Option<i64>) -> Markup {
    html! {
        option value="" selected[selected.is_none()] { "None (top level)" }
        @for (depth, doc) in documents {
            option value=(doc.id) selected[selected == Some(doc.id)] {
                (" \u{b7} ".repeat(*depth)) (doc.title)
            }
        }
    }
}

fn document_tree(data: &DocumentsData) -> Markup {
    let key = data.project.key.as_str();
    html! {
        section #doc-tree aria-label="Documents" {
            @if data.documents.is_empty() {
                p.empty { "No documents yet. Create the first one below." }
            } @else {
                ul.doc-tree {
                    @for (depth, doc) in &data.documents {
                        li data-depth=(depth) {
                            a href={ "/p/" (key) "/documents/" (doc.id) } { (doc.title) }
                            " "
                            span.meta {
                                "updated "
                                time.when datetime=(utc_iso(doc.updated)) data-ts=(doc.updated) {
                                    (utc_text(doc.updated))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The tree alone; also what `GET /p/{key}/documents/list` returns for live refreshes.
pub fn documents_fragment(data: &DocumentsData) -> Markup {
    document_tree(data)
}

pub fn documents_page(data: &DocumentsData) -> Markup {
    let key = data.project.key.as_str();
    let mut frame = Frame::new("Documents", "documents");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    layout(
        &frame,
        html! {
            section.page {
                nav.crumbs aria-label="Breadcrumb" {
                    a href={ "/p/" (key) } { (data.project.name) }
                    " / "
                    span { "Documents" }
                }
                h1 { "Documents" }
                (document_tree(data))
                h2 { "New document" }
                form.inline-form data-form="new-document" {
                    div.field {
                        label for="document-title" { "Title" }
                        input #document-title name="title" required maxlength="200"
                            placeholder="Design notes" autocomplete="off";
                    }
                    div.field {
                        label for="document-parent" { "Parent" }
                        select #document-parent name="parent" {
                            (document_options(&data.documents, data.parent))
                        }
                    }
                    button.primary type="submit" { "Create document" }
                    p.field-msg role="alert" {}
                }
            }
        },
    )
}

fn links_of_document(links: &DocumentLinks, key: &str, archived: bool) -> Markup {
    let none = links.issues.is_empty() && links.initiatives.is_empty();
    html! {
        section #doc-links aria-labelledby="doc-links-heading" {
            h2 #doc-links-heading { "Linked to" }
            @if none {
                p.empty { "Not linked to any issue or initiative." }
            } @else {
                ul.links-list {
                    @for issue in &links.issues {
                        li {
                            a href={ "/p/" (key) "/" (issue.number) } {
                                span.ident { (issue.identifier) }
                                " "
                                (issue.title)
                            }
                            " "
                            button type="button" data-action="remove-doc-link" data-link-id=(issue.link_id) {
                                "Unlink"
                                span.sr-only { " " (issue.identifier) }
                            }
                        }
                    }
                    @for initiative in &links.initiatives {
                        li {
                            a href={ "/p/" (key) "/initiatives/" (initiative.id) } {
                                span.chip { "initiative" }
                                " "
                                (initiative.name)
                            }
                            " "
                            button type="button" data-action="remove-doc-link" data-link-id=(initiative.link_id) {
                                "Unlink"
                                span.sr-only { " " (initiative.name) }
                            }
                        }
                    }
                }
            }
            @if !archived {
                form.inline-form data-form="add-doc-link" {
                    h3 { "Link to an issue or initiative" }
                    div.field {
                        label for="doc-link-kind" { "Kind" }
                        select #doc-link-kind name="kind" {
                            option value="issue" { "Issue number" }
                            option value="initiative" { "Initiative id" }
                        }
                    }
                    div.field {
                        label for="doc-link-target" { "Number" }
                        input #doc-link-target name="target" type="number" min="1"
                            inputmode="numeric" required;
                    }
                    button type="submit" { "Add link" }
                    p.field-msg role="alert" {}
                }
            }
        }
    }
}

/// The whole document, editor included; also what `GET /p/{key}/documents/{id}/content`
/// returns for live refreshes. `data-revision` is the revision a save is based on.
pub fn document_fragment(data: &DocumentData) -> Markup {
    let key = data.project.key.as_str();
    let doc = &data.detail.document;
    html! {
        article #doc data-revision=(doc.revision) data-archived=(doc.archived) {
            div.issue-head {
                h1 #doc-title { (doc.title) }
                @if doc.archived { span.chip.chip-archived { "archived" } }
                @if !doc.archived {
                    button type="button" data-action="edit-document" { "Edit" }
                }
                @if doc.archived {
                    button type="button" data-action="unarchive-document" { "Restore" }
                } @else {
                    button type="button" data-action="archive-document" { "Archive" }
                }
                a.button href={ "/p/" (key) "/documents/" (doc.id) "/history" } {
                    "History (" (doc.revision) ")"
                }
            }
            p.meta {
                "Revision " (doc.revision) ", updated "
                time.when datetime=(utc_iso(doc.updated)) data-ts=(doc.updated) {
                    (utc_text(doc.updated))
                }
                "."
            }
            p.field-msg #page-msg role="alert" {}
            section.markdown #doc-body aria-label="Document" {
                @if doc.body.trim().is_empty() {
                    p.empty { "This document is empty." }
                } @else {
                    (markdown::render(&doc.body))
                }
            }
            @if !doc.archived {
                form.doc-editor #doc-editor data-form="save-document" hidden {
                    div.field {
                        label for="doc-edit-title" { "Title" }
                        input #doc-edit-title name="title" value=(doc.title) required maxlength="200";
                    }
                    div.field {
                        label for="doc-edit-body" { "Body (Markdown)" }
                        textarea #doc-edit-body name="body" rows="20" maxlength="200000" {
                            "\n" (doc.body)
                        }
                    }
                    div.row {
                        button.primary type="submit" { "Save" }
                        button type="button" data-action="preview-document" { "Preview" }
                        button type="button" data-action="cancel-edit" { "Cancel" }
                        button type="button" data-action="reload-page" #doc-reload hidden {
                            "Reload the latest version"
                        }
                    }
                    p.field-msg role="alert" {}
                    div.markdown #doc-preview aria-label="Preview" hidden {}
                }
            }
            section #doc-children aria-labelledby="doc-children-heading" {
                h2 #doc-children-heading { "Sub-documents" }
                @if data.detail.children.is_empty() {
                    p.empty { "No sub-documents." }
                } @else {
                    ul.links-list {
                        @for child in &data.detail.children {
                            li { a href={ "/p/" (key) "/documents/" (child.id) } { (child.title) } }
                        }
                    }
                }
                @if !doc.archived {
                    p {
                        a href={ "/p/" (key) "/documents?parent=" (doc.id) } { "Add a sub-document" }
                    }
                }
            }
            (links_of_document(&data.detail.links, key, doc.archived))
        }
    }
}

fn document_crumbs(project: &Project, detail: &DocumentDetail) -> Markup {
    let key = project.key.as_str();
    html! {
        nav.crumbs aria-label="Breadcrumb" {
            a href={ "/p/" (key) } { (project.name) }
            " / "
            a href={ "/p/" (key) "/documents" } { "Documents" }
            @for ancestor in &detail.ancestors {
                " / "
                a href={ "/p/" (key) "/documents/" (ancestor.id) } { (ancestor.title) }
            }
            " / "
            a href={ "/p/" (key) "/documents/" (detail.document.id) } { (detail.document.title) }
        }
    }
}

pub fn document_page(data: &DocumentData) -> Markup {
    let doc = &data.detail.document;
    let mut frame = Frame::new(&doc.title, "document");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    frame.document_id = Some(doc.id);
    layout(
        &frame,
        html! {
            section.page.doc-page {
                (document_crumbs(&data.project, &data.detail))
                (document_fragment(data))
            }
        },
    )
}

// ---- agents ----

/// The fields of the create and edit forms. `agent` is `None` on the create form.
fn agent_fields(agent: Option<&Agent>, projects: &[Project]) -> Markup {
    let private = agent.and_then(|a| a.private.as_ref());
    let args = private.map(|p| p.args.join("\n")).unwrap_or_default();
    let max_concurrent = agent.map_or(1, |a| a.max_concurrent);
    let timeout = agent.map_or(3600, |a| a.timeout_seconds);
    let on_assign = agent.is_none_or(|a| a.on_assign);
    let on_mention = agent.is_none_or(|a| a.on_mention);
    html! {
        @if agent.is_none() {
            div.field {
                label for="a-name" { "Name" }
                input #a-name name="name" required maxlength="100" autocomplete="off"
                    placeholder="claude-code";
            }
        }
        div.field {
            label for="a-description" { "Description" }
            input #a-description name="description" maxlength="1000" autocomplete="off"
                value=[agent.map(|a| a.description.as_str())];
        }
        div.field {
            label for="a-command" { "Command" }
            input #a-command name="command" required maxlength="500" autocomplete="off"
                placeholder="claude" value=[private.map(|p| p.command.as_str())];
        }
        div.field {
            label for="a-args" { "Arguments, one per line" }
            textarea #a-args name="args" rows="3" spellcheck="false" { "\n" (args) }
        }
        div.field {
            label for="a-model" { "Model (optional)" }
            input #a-model name="model" maxlength="200" autocomplete="off"
                value=[private.and_then(|p| p.model.as_deref())];
        }
        div.field {
            label for="a-work-dir" { "Working directory (optional, absolute path)" }
            input #a-work-dir name="work_dir" maxlength="1000" autocomplete="off"
                value=[private.and_then(|p| p.work_dir.as_deref())];
        }
        div.field {
            label for="a-instructions" { "Instructions" }
            textarea #a-instructions name="instructions" rows="6" maxlength="20000"
                data-prompt-field {
                "\n" (private.map_or("", |p| p.instructions.as_str()))
            }
        }
        fieldset.field.checks {
            legend { "Start a run when" }
            label.check {
                input type="checkbox" name="on_assign" checked[on_assign];
                " an issue is assigned to this agent"
            }
            label.check {
                input type="checkbox" name="on_mention" checked[on_mention];
                " this agent is mentioned in a comment"
            }
        }
        div.field {
            label for="a-max" { "Max concurrent runs (1-16)" }
            input #a-max name="max_concurrent" type="number" min="1" max="16" value=(max_concurrent);
        }
        div.field {
            label for="a-timeout" { "Timeout in seconds (60-86400)" }
            input #a-timeout name="timeout_seconds" type="number" min="60" max="86400"
                value=(timeout);
        }
        fieldset.field.checks {
            legend { "Allowed projects" }
            p.hint {
                "An agent with no project ticked can access no project at all."
            }
            @if projects.is_empty() {
                p.empty { "There are no projects yet." }
            }
            @for project in projects {
                label.check {
                    input type="checkbox" name="project" value=(project.key)
                        checked[agent.is_some_and(|a| a.projects.contains(&project.key))];
                    " "
                    span.key { (project.key) }
                    " "
                    (project.name)
                }
            }
        }
    }
}

pub fn agents_page(data: &AgentsData) -> Markup {
    let mut frame = Frame::new("Agents", "agents");
    frame.user = Some(&data.user);
    layout(
        &frame,
        html! {
            section.page {
                h1 { "Agents" }
                div #key-box.key-box hidden role="status" {
                    p { strong { "Copy this key now. It is shown once and cannot be shown again." } }
                    p { code #key-value {} }
                    p.hint { "Add it to Claude Code with:" }
                    p { code #key-command {} }
                    button type="button" data-action="copy-key" { "Copy key" }
                }
                @if data.agents.is_empty() {
                    p.empty { "No agents yet." }
                } @else {
                    ul.agent-list {
                        @for agent in &data.agents {
                            li {
                                a href={ "/agents/" (agent.id) } { (agent.name) }
                                @if !agent.enabled { " " span.chip { "disabled" } }
                                " "
                                span.hint {
                                    @if agent.projects.is_empty() {
                                        "no projects"
                                    } @else {
                                        (agent.projects.join(", "))
                                    }
                                }
                            }
                        }
                    }
                }
                @if data.owner {
                    h2 { "New agent" }
                    form.agent-form data-form="new-agent" {
                        (agent_fields(None, &data.projects))
                        label.check {
                            input type="checkbox" name="create_key" checked;
                            " Create a key for this agent"
                        }
                        div.row { button.primary type="submit" { "Create agent" } }
                        p.field-msg role="alert" {}
                    }
                } @else {
                    p.hint { "Only the owner can create or edit agents." }
                }
            }
        },
    )
}

fn source_label(source: TemplateSource) -> &'static str {
    match source {
        TemplateSource::Builtin => "the built-in default",
        TemplateSource::Agent => "the instructions of this agent",
        TemplateSource::Project => "an override for the project",
        TemplateSource::ProjectType => "an override for the project and issue type",
        TemplateSource::Inline => "the text you typed",
    }
}

/// What a click on a variable inserts: the variable, or for a list a loop.
fn variable_snippet(variable: &VariableInfo) -> String {
    match (variable.kind, variable.item) {
        ("list", Some("text")) => {
            format!("{{{{#each {}}}}}{{{{this}}}} {{{{/each}}}}", variable.name)
        }
        ("list", _) => format!("{{{{#each {}}}}}\n\n{{{{/each}}}}", variable.name),
        _ => format!("{{{{{}}}}}", variable.name),
    }
}

/// The "Prompt" section of the agent page. Owner only: the caller does not
/// build it for anyone else, because template text is as private as the
/// agent's instructions.
fn prompt_section(projects: &[Project], prompt: &PromptData) -> Markup {
    html! {
        section.prompt #prompt {
            h2 { "Prompt" }
            p {
                "The first message of a run is written from "
                strong { (source_label(prompt.default_source)) }
                " unless an override below applies."
            }
            p.hint {
                "The most specific text wins: an override for a project and issue type, then "
                "one for the project, then the Instructions field above (when it is not empty), "
                "then the built-in default. Use the variables below to put the issue, the "
                "comments and the trigger into the message."
            }

            h3 { "Overrides" }
            @if prompt.templates.is_empty() {
                p.empty { "No overrides." }
            } @else {
                ul.template-list {
                    @for template in &prompt.templates {
                        li {
                            span.template-scope {
                                (template.project)
                                @if let Some(name) = &template.issue_type {
                                    ", " (name)
                                } @else {
                                    ", any issue type"
                                }
                            }
                            " "
                            span.meta { (template.body.chars().count()) " characters" }
                            " "
                            button type="button" data-action="edit-template"
                                data-project=(template.project)
                                data-type-id=(template.issue_type_id.map(|id| id.to_string()).unwrap_or_default())
                                data-body=(template.body) { "Edit" }
                            button type="button" data-action="delete-template"
                                data-template-id=(template.id) { "Delete" }
                        }
                    }
                }
            }
            form.prompt-form data-form="save-template" {
                div.field {
                    label for="t-project" { "Project" }
                    select #t-project name="project" required {
                        @for project in projects {
                            option value=(project.key) { (project.key) " \u{2013} " (project.name) }
                        }
                    }
                }
                div.field {
                    label for="t-type" { "Issue type" }
                    select #t-type name="issue_type_id" {
                        option value="" { "Any issue type" }
                        @for (key, types) in &prompt.issue_types {
                            @for kind in types {
                                option value=(kind.id) data-project=(key) { (key) ": " (kind.name) }
                            }
                        }
                    }
                }
                div.field {
                    label for="t-body" { "Override text" }
                    textarea #t-body name="body" rows="10" maxlength="20000" required
                        spellcheck="false" data-prompt-field { "\n" }
                }
                div.row {
                    button.primary type="submit" { "Save override" }
                    button type="button" data-action="preview-prompt" data-source="#t-body" {
                        "Preview this text"
                    }
                    button type="button" data-action="clear-template-form" { "Clear" }
                }
                p.field-msg role="alert" {}
            }

            h3 { "Preview" }
            p.hint {
                "Shows the exact message a run would start with, for one issue. Nothing is "
                "started or saved. The working copy and the run id are empty in a preview."
            }
            div.prompt-preview {
                div.field {
                    label for="pv-project" { "Project" }
                    select #pv-project {
                        @for project in projects {
                            option value=(project.key) { (project.key) }
                        }
                    }
                }
                div.field {
                    label for="pv-number" { "Issue number" }
                    input #pv-number type="number" min="1" value="1" inputmode="numeric";
                }
                div.field {
                    label for="pv-trigger" { "Started because" }
                    select #pv-trigger {
                        option value="manual" { "a person started it" }
                        option value="assigned" { "the issue was assigned" }
                        option value="mentioned" { "the agent was mentioned" }
                        option value="schedule" { "a schedule" }
                    }
                }
                div.row {
                    button type="button" data-action="preview-prompt" data-source="stored" {
                        "Preview as saved"
                    }
                    button type="button" data-action="preview-prompt" data-source="#a-instructions" {
                        "Preview the Instructions text"
                    }
                }
                p.field-msg #pv-msg role="alert" {}
                p.meta #pv-meta {}
                pre.prompt-text #pv-text hidden {}
            }

            h3 { "Variables" }
            p.hint {
                "Click a variable to insert it at the cursor of the text field you used last. "
                "Write {{#if name}}...{{else}}...{{/if}} for a condition and "
                "{{#each list}}...{{/each}} for a list; a value is never read as template text."
            }
            ul.var-list {
                @for variable in &prompt.variables {
                    li {
                        button.var type="button" data-action="insert-variable"
                            data-text=(variable_snippet(variable)) title=(variable.description) {
                            code { (variable.name) }
                        }
                        " "
                        span.meta { (variable.kind) ": " (variable.description) }
                        @if variable.kind != "list" {
                            " "
                            button.var type="button" data-action="insert-variable"
                                data-text=(format!("{{{{#if {}}}}}\n\n{{{{/if}}}}", variable.name)) {
                                "if"
                            }
                        }
                        @for field in &variable.fields {
                            " "
                            button.var type="button" data-action="insert-variable"
                                data-text=(format!("{{{{this.{}}}}}", field.name))
                                title=(field.description) {
                                code { "this." (field.name) }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn agent_page(data: &AgentData) -> Markup {
    let agent = &data.agent;
    let mut frame = Frame::new(&agent.name, "agent");
    frame.user = Some(&data.user);
    layout(
        &frame,
        html! {
            article.page data-agent-id=(agent.id) {
                nav.crumbs aria-label="Breadcrumb" {
                    a href="/agents" { "Agents" }
                    " / "
                    span { (agent.name) }
                }
                div.issue-head {
                    h1 { (agent.name) }
                    @if !agent.enabled { span.chip { "disabled" } }
                }
                p.field-msg #page-msg role="alert" {}
                @if data.owner {
                    div.row {
                        @if agent.enabled {
                            button type="button" data-action="disable-agent" { "Disable" }
                        } @else {
                            button type="button" data-action="enable-agent" { "Enable" }
                        }
                        button type="button" data-action="delete-agent" { "Delete" }
                    }
                    form.agent-form data-form="edit-agent" {
                        (agent_fields(Some(agent), &data.projects))
                        div.row { button.primary type="submit" { "Save" } }
                        p.field-msg role="alert" {}
                    }
                    @if let Some(prompt) = &data.prompt {
                        (prompt_section(&data.projects, prompt))
                    }
                    (run_now_section(agent))
                    (queue_panel(data))
                    h2 { "Keys" }
                    @if data.keys.is_empty() {
                        p.empty { "This agent has no keys." }
                    } @else {
                        ul.key-list {
                            @for key in &data.keys {
                                li {
                                    span { (key.name) }
                                    " "
                                    code { (key.prefix) "\u{2026}" }
                                    @if key.revoked.is_some() {
                                        " " span.chip { "revoked" }
                                    } @else {
                                        " "
                                        button type="button" data-action="revoke-key"
                                            data-key-id=(key.id) { "Revoke" }
                                    }
                                }
                            }
                        }
                    }
                    div #key-box.key-box hidden role="status" {
                        p { strong { "Copy this key now. It is shown once and cannot be shown again." } }
                        p { code #key-value {} }
                        p.hint { "Add it to Claude Code with:" }
                        p { code #key-command {} }
                        button type="button" data-action="copy-key" { "Copy key" }
                    }
                    button type="button" data-action="new-key"
                        data-user-id=(agent.user_id) { "Create a new key" }
                } @else {
                    dl.facts {
                        dt { "Description" } dd { (agent.description) }
                        dt { "Runs when" }
                        dd {
                            @if agent.on_assign { "assigned " }
                            @if agent.on_mention { "mentioned" }
                            @if !agent.on_assign && !agent.on_mention { "never" }
                        }
                        dt { "Max concurrent runs" } dd { (agent.max_concurrent) }
                        dt { "Timeout" } dd { (agent.timeout_seconds) " seconds" }
                        dt { "Allowed projects" }
                        dd {
                            @if agent.projects.is_empty() { "none" } @else { (agent.projects.join(", ")) }
                        }
                    }
                    (queue_panel(data))
                    p.hint { "Only the owner can edit agents." }
                }
            }
        },
    )
}

/// Running against the limits, and the runs waiting for a slot, oldest first.
/// Static: it shows the state when the page was loaded.
fn queue_panel(data: &AgentData) -> Markup {
    let agent = &data.agent;
    html! {
        section.queue-panel aria-labelledby="queue-heading" {
            h2 #queue-heading { "Queue" }
            p.queue-summary {
                (data.queue.running) " running of " (agent.max_concurrent)
                " allowed for this agent"
                @if let Some(limit) = data.max_runs {
                    ", overall limit " (limit) " runs at once"
                }
                "."
            }
            @if data.queue.queued.is_empty() {
                p.empty { "No run is waiting." }
            } @else {
                ol.run-list {
                    @for entry in &data.queue.queued {
                        li {
                            span.chip.chip-run data-status="queued" {
                                "Queued (position " (entry.position) ")"
                            }
                            " "
                            a.run-link href={ "/p/" (entry.project) "/runs/" (entry.run_id) } {
                                "Run " (entry.run_id)
                            }
                            " "
                            a href={ "/p/" (entry.project) "/" (entry.issue.rsplit('-').next().unwrap_or("")) } {
                                (entry.issue)
                            }
                            " "
                            span.meta {
                                @if entry.waiting_on == "agent" {
                                    "waiting for this agent's limit"
                                } @else {
                                    "waiting for a free slot overall"
                                }
                            }
                            " "
                            (run_time("queued", Some(entry.queued_at)))
                            " "
                            button type="button" data-action="cancel-run" data-run-id=(entry.run_id) {
                                "Cancel"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Owner only, like the rest of the agent controls; members start runs
/// through the API.
fn run_now_section(agent: &Agent) -> Markup {
    html! {
        section.run-now aria-labelledby="run-now-heading" {
            h2 #run-now-heading { "Run on an issue now" }
            @if !agent.enabled {
                p.hint { "The agent is disabled. Enable it to start a run." }
            } @else if agent.projects.is_empty() {
                p.hint { "The agent has no allowed project. Allow one to start a run." }
            } @else {
                form.agent-form data-form="start-run" {
                    div.field {
                        label for="rn-project" { "Project" }
                        select #rn-project name="project" {
                            @for key in &agent.projects { option value=(key) { (key) } }
                        }
                    }
                    div.field {
                        label for="rn-number" { "Issue number" }
                        input #rn-number name="number" type="number" min="1"
                            inputmode="numeric" required;
                    }
                    div.field {
                        label for="rn-message" { "First message (optional)" }
                        textarea #rn-message name="message" rows="8" {}
                    }
                    p.hint {
                        "Preview fills the box with exactly what the agent would get. "
                        "Edit it if you like. Left empty or unchanged, the stored template is used."
                    }
                    div.row {
                        button type="button" data-action="preview-run" { "Preview" }
                        button.primary type="submit" { "Start run" }
                    }
                    p.field-msg role="alert" {}
                }
            }
        }
    }
}

pub fn history_page(data: &HistoryData) -> Markup {
    let key = data.project.key.as_str();
    let doc = &data.detail.document;
    let title = format!("History of {}", doc.title);
    let mut frame = Frame::new(&title, "history");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.document_id = Some(doc.id);
    layout(
        &frame,
        html! {
            section.page {
                (document_crumbs(&data.project, &data.detail))
                h1 { "History" }
                ol.revisions reversed {
                    @for revision in &data.revisions {
                        li {
                            a href={ "/p/" (key) "/documents/" (doc.id) "/history/" (revision.number) } {
                                "Revision " (revision.number)
                            }
                            " "
                            @if revision.number == doc.revision {
                                span.chip { "current" }
                                " "
                            }
                            span.meta {
                                (revision.title) ", " (revision.chars) " characters, by "
                                (revision.author.as_deref().unwrap_or("unknown")) " "
                                time.when datetime=(utc_iso(revision.created)) data-ts=(revision.created) {
                                    (utc_text(revision.created))
                                }
                                @if let Some(note) = &revision.note { " \u{2013} " (note) }
                            }
                        }
                    }
                }
            }
        },
    )
}

pub fn revision_page(data: &RevisionData) -> Markup {
    let key = data.project.key.as_str();
    let doc = &data.detail.document;
    let revision = &data.revision;
    let title = format!("{} revision {}", doc.title, revision.number);
    let mut frame = Frame::new(&title, "revision");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.document_id = Some(doc.id);
    layout(
        &frame,
        html! {
            section.page {
                (document_crumbs(&data.project, &data.detail))
                nav.crumbs aria-label="History" {
                    a href={ "/p/" (key) "/documents/" (doc.id) "/history" } { "History" }
                    " / "
                    span { "Revision " (revision.number) }
                }
                div.issue-head {
                    h1 { (revision.title) }
                    @if revision.number == doc.revision {
                        span.chip { "current" }
                    } @else if !doc.archived {
                        button type="button" data-action="restore-revision"
                            data-revision=(revision.number) data-base-revision=(doc.revision) {
                            "Restore this revision"
                        }
                    }
                }
                p.meta {
                    "Revision " (revision.number) " by "
                    (revision.author.as_deref().unwrap_or("unknown")) " "
                    time.when datetime=(utc_iso(revision.created)) data-ts=(revision.created) {
                        (utc_text(revision.created))
                    }
                    @if let Some(note) = &revision.note { " \u{2013} " (note) }
                }
                p.field-msg #page-msg role="alert" {}
                section.markdown aria-label="Document" {
                    (markdown::render(&revision.body))
                }
            }
        },
    )
}

// ---- issue ----

fn json_label(field: &str, value: &Value, lookup: &Lookup<'_>) -> String {
    let none = || "none".to_string();
    match (field, value) {
        (_, Value::Null) => none(),
        ("type_id", v) => v.as_i64().map_or_else(none, |id| lookup.type_name(id)),
        ("assignee_id", v) => v.as_i64().map_or_else(none, |id| lookup.user(id)),
        ("parent", v) => v.as_i64().map_or_else(none, |n| format!("#{n}")),
        ("initiative", v) => v.as_i64().map_or_else(none, |id| lookup.initiative(id)),
        ("labels", Value::Array(ids)) => {
            let names: Vec<String> = ids
                .iter()
                .filter_map(Value::as_i64)
                .map(|id| lookup.label(id))
                .collect();
            if names.is_empty() {
                none()
            } else {
                names.join(", ")
            }
        }
        (_, Value::String(text)) => text.clone(),
        (_, other) => other.to_string(),
    }
}

fn field_name(field: &str) -> &str {
    match field {
        "type_id" => "type",
        "assignee_id" => "assignee",
        other => other,
    }
}

/// A sentence for what happened and one line for each changed field.
fn describe(event: &Event, lookup: &Lookup<'_>) -> (String, Vec<String>) {
    let payload = &event.payload;
    match event.kind.as_str() {
        "issue.created" => ("created the issue".to_string(), Vec::new()),
        "issue.moved" => {
            let column = |side: &str| {
                payload[side]["column_id"]
                    .as_i64()
                    .map_or_else(|| "?".to_string(), |id| lookup.column(id))
            };
            let from = column("from");
            let to = column("to");
            if from == to {
                ("reordered the issue".to_string(), Vec::new())
            } else {
                (format!("moved the issue from {from} to {to}"), Vec::new())
            }
        }
        "link.added" | "link.removed" => {
            let verb = if event.kind == "link.added" {
                "added"
            } else {
                "removed"
            };
            let ident = |side: &str| payload[side]["identifier"].as_str().unwrap_or("?");
            let relation = if payload["kind"] == "blocks" {
                "blocks"
            } else {
                "relates to"
            };
            (
                format!(
                    "{verb} the link {} {relation} {}",
                    ident("from"),
                    ident("to")
                ),
                Vec::new(),
            )
        }
        "comment.created" => {
            let text = if payload["parent"].is_null() {
                "commented"
            } else {
                "replied to a comment"
            };
            (text.to_string(), Vec::new())
        }
        "comment.edited" => ("edited a comment".to_string(), Vec::new()),
        "comment.deleted" => ("deleted a comment".to_string(), Vec::new()),
        "issue.archived" => ("archived the issue".to_string(), Vec::new()),
        "issue.unarchived" => ("restored the issue".to_string(), Vec::new()),
        "issue.updated" => {
            let lines = payload["changes"]
                .as_object()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|(field, change)| {
                            if field == "description" {
                                "description edited".to_string()
                            } else {
                                format!(
                                    "{}: {} \u{2192} {}",
                                    field_name(field),
                                    json_label(field, &change["from"], lookup),
                                    json_label(field, &change["to"], lookup)
                                )
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            ("updated the issue".to_string(), lines)
        }
        other => (other.to_string(), Vec::new()),
    }
}

fn activity_list(events: &[Event], lookup: &Lookup<'_>) -> Markup {
    // A mention is for the runner and the mentioned user; the comment line says enough.
    let shown: Vec<&Event> = events
        .iter()
        .rev()
        .filter(|e| e.kind != "mention.created")
        .collect();
    html! {
        section #activity.activity aria-labelledby="activity-heading" {
            h2 #activity-heading { "Activity" }
            @if shown.is_empty() {
                p.empty { "Nothing yet." }
            } @else {
                ol.activity-list {
                    @for event in shown {
                        @let (headline, lines) = describe(event, lookup);
                        li data-kind=(event.kind) {
                            p {
                                strong { (event.actor.map_or_else(|| "someone".to_string(), |id| lookup.user(id))) }
                                " "
                                (headline)
                                " "
                                time.when datetime=(utc_iso(event.created)) data-ts=(event.created) {
                                    (utc_text(event.created))
                                }
                            }
                            @if !lines.is_empty() {
                                ul.changes {
                                    @for line in &lines { li { (line) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn link_group(title: &str, items: &[LinkedIssue], key: &str, lookup: &Lookup<'_>) -> Markup {
    html! {
        @if !items.is_empty() {
            h3 { (title) }
            ul.links-list {
                @for item in items {
                    li {
                        a href={ "/p/" (key) "/" (item.number) } {
                            span.ident { (item.identifier) }
                            " "
                            (item.title)
                        }
                        " "
                        span.chip data-state=(item.state_type.as_str()) { (lookup.column(item.column_id)) }
                        @if item.archived { " " span.chip.chip-archived { "archived" } }
                        " "
                        button type="button" data-action="remove-link" data-link-id=(item.link_id) {
                            "Remove"
                            span.sr-only { " link to " (item.identifier) }
                        }
                    }
                }
            }
        }
    }
}

fn links_section(links: &IssueLinks, blocked: bool, key: &str, lookup: &Lookup<'_>) -> Markup {
    let none = links.blocks.is_empty() && links.blocked_by.is_empty() && links.related.is_empty();
    html! {
        section #links aria-labelledby="links-heading" {
            h2 #links-heading { "Links" }
            @if blocked {
                p.blocked-note { strong { "Blocked" } " until its blockers are done." }
            }
            @if none {
                p.empty { "No links." }
            }
            (link_group("Blocks", &links.blocks, key, lookup))
            (link_group("Blocked by", &links.blocked_by, key, lookup))
            (link_group("Related", &links.related, key, lookup))
            form.inline-form data-form="add-link" {
                h3 { "Add a link" }
                div.field {
                    label for="link-kind" { "Link type" }
                    select #link-kind name="kind" {
                        option value="blocks" { "Blocks" }
                        option value="blocked_by" { "Is blocked by" }
                        option value="relates" { "Relates to" }
                    }
                }
                div.field {
                    label for="link-to" { "Issue number" }
                    input #link-to name="to" type="number" min="1" inputmode="numeric" required;
                }
                button type="submit" { "Add link" }
                p.field-msg role="alert" {}
            }
            p.field-msg #links-msg role="alert" {}
        }
    }
}

/// The body split at the mentions it resolved to: `(is_mention, text)`.
fn body_pieces(comment: &Comment) -> Vec<(bool, &str)> {
    let names: Vec<&str> = comment.mentions.iter().map(|m| m.name.as_str()).collect();
    let mut pieces = Vec::new();
    let mut at = 0;
    for range in mention_ranges(&comment.body, &names) {
        if range.start > at {
            pieces.push((false, &comment.body[at..range.start]));
        }
        pieces.push((true, &comment.body[range.start..range.end]));
        at = range.end;
    }
    if at < comment.body.len() {
        pieces.push((false, &comment.body[at..]));
    }
    pieces
}

fn comment_form(
    kind: &'static str,
    label: &str,
    initial: &str,
    parent: Option<i64>,
    comment: Option<i64>,
    submit: &str,
) -> Markup {
    let id = match (comment, parent) {
        (Some(id), _) => format!("edit-{id}"),
        (None, Some(id)) => format!("reply-{id}"),
        (None, None) => "new-comment".to_string(),
    };
    let class = match kind {
        "edit-comment" => "comment-edit",
        "add-reply" => "reply-form",
        _ => "comment-form",
    };
    html! {
        form class=(class) data-form=(kind) data-parent-id=[parent] data-comment-id=[comment]
            hidden[kind != "add-comment"] {
            div.field {
                label for=(id) { (label) }
                textarea id=(id) name="body" rows="4" required maxlength="20000" {
                    "\n" (initial)
                }
            }
            div.row {
                button.primary type="submit" { (submit) }
                @if kind != "add-comment" {
                    button type="button" data-action="hide-comment-form" { "Cancel" }
                }
            }
            p.field-msg role="alert" {}
        }
    }
}

fn comment_item(comment: &Comment, viewer: &User) -> Markup {
    let may_change = comment.author.id == viewer.id || viewer.role == Role::Owner;
    let top_level = comment.parent.is_none();
    let deleted = comment.deleted.is_some();
    html! {
        li.comment.reply[!top_level] id={ "comment-" (comment.id) } data-comment-id=(comment.id) {
            div.comment-head {
                strong { (comment.author.name) }
                @if comment.author.kind == "agent" { span.chip { "agent" } }
                time.when datetime=(utc_iso(comment.created)) data-ts=(comment.created) {
                    (utc_text(comment.created))
                }
                @if let (Some(edited), false) = (comment.edited, deleted) {
                    span.comment-edited title=(utc_text(edited)) { "edited" }
                }
            }
            @if deleted {
                p.comment-deleted { "This comment was deleted." }
            } @else {
                p.comment-text {
                    @for (is_mention, piece) in body_pieces(comment) {
                        @if is_mention { span.mention { (piece) } } @else { (piece) }
                    }
                }
                div.comment-actions {
                    @if top_level {
                        button type="button" data-action="show-reply" {
                            "Reply"
                            span.sr-only { " to " (comment.author.name) }
                        }
                    }
                    @if may_change {
                        button type="button" data-action="show-edit" {
                            "Edit"
                            span.sr-only { " comment by " (comment.author.name) }
                        }
                        button type="button" data-action="delete-comment" {
                            "Delete"
                            span.sr-only { " comment by " (comment.author.name) }
                        }
                    }
                }
                @if may_change {
                    (comment_form("edit-comment", "Edit your comment", &comment.body, None, Some(comment.id), "Save"))
                }
            }
            @if top_level {
                @if !comment.replies.is_empty() {
                    ol.reply-list {
                        @for reply in &comment.replies { (comment_item(reply, viewer)) }
                    }
                }
                @if !deleted {
                    (comment_form("add-reply", "Reply", "", Some(comment.id), None, "Reply"))
                }
            }
        }
    }
}

fn comments_list(comments: &[Comment], viewer: &User) -> Markup {
    html! {
        section #comments.comments aria-labelledby="comments-heading" {
            h2 #comments-heading { "Comments" }
            @if comments.is_empty() {
                p.empty { "No comments yet." }
            } @else {
                ol.comment-list {
                    @for comment in comments { (comment_item(comment, viewer)) }
                }
            }
            (comment_form("add-comment", "Add a comment", "", None, None, "Comment"))
        }
    }
}

pub fn comments_fragment(data: &CommentsData) -> Markup {
    comments_list(&data.comments, &data.viewer)
}

pub fn activity_fragment(data: &ActivityData) -> Markup {
    let lookup = Lookup::new(
        &data.columns,
        &data.types,
        &data.labels,
        &data.users,
        &data.initiatives,
    );
    activity_list(&data.activity, &lookup)
}

// ---- runs ----

fn run_chip(run: &Run) -> Markup {
    html! {
        span.chip.chip-run data-status=(run.status.as_str()) {
            @if let (RunStatus::Queued, Some(position)) = (run.status, run.queue_position) {
                "Queued (position " (position) ")"
            } @else {
                (run.status.as_str())
            }
        }
    }
}

/// What became of the run's working copy, in words (never colour alone).
fn worktree_text(run: &Run) -> &'static str {
    match run.worktree_state {
        WorktreeState::None => "no working copy of its own",
        WorktreeState::Created if run.status.is_active() => "worktree in use",
        WorktreeState::Created => "worktree not settled yet",
        WorktreeState::Removed => "worktree removed \u{2014} no changes",
        WorktreeState::Kept if run.has_changes => "worktree kept \u{2014} has changes",
        WorktreeState::Kept => "worktree kept \u{2014} not inspected",
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn worktree_chip(run: &Run) -> Markup {
    html! {
        @if run.worktree_state != WorktreeState::None {
            span.chip.chip-worktree data-state=(run.worktree_state.as_str())
                data-changes=(run.has_changes) {
                (worktree_text(run))
            }
        }
    }
}

fn run_time(label: &str, at: Option<i64>) -> Markup {
    html! {
        @if let Some(at) = at {
            span.meta {
                (label) " "
                time.when datetime=(utc_iso(at)) data-ts=(at) { (utc_text(at)) }
            }
        }
    }
}

fn runs_list(key: &str, runs: &[Run]) -> Markup {
    // The newest active run is the one whose output the issue page follows.
    let live = runs
        .iter()
        .find(|run| run.status.is_active())
        .map(|run| run.id);
    html! {
        section #runs.runs aria-labelledby="runs-heading" data-live-run=[live] {
            h2 #runs-heading { "Runs" }
            @if runs.is_empty() {
                p.empty { "No runs yet." }
            } @else {
                ul.run-list {
                    @for run in runs {
                        li {
                            a.run-link href={ "/p/" (key) "/runs/" (run.id) } { "Run " (run.id) }
                            " "
                            (run_chip(run))
                            " "
                            span { (run.agent) }
                            " "
                            span.meta { (run.reason.as_str()) }
                            " "
                            @if run.status == RunStatus::Queued {
                                (run_time("queued", run.queued_at.or(Some(run.created))))
                            } @else {
                                (run_time("started", run.started.or(Some(run.created))))
                            }
                            (run_time("finished", run.finished))
                            @if let Some(code) = run.exit_code {
                                " " span.meta { "exit " (code) }
                            }
                            @if run.status.is_active() {
                                " "
                                button type="button" data-action="cancel-run" data-run-id=(run.id) {
                                    "Cancel"
                                }
                            }
                            @if run.worktree_state != WorktreeState::None {
                                " " (worktree_chip(run))
                                @if let Some(branch) = &run.branch {
                                    " " code.branch { (branch) }
                                }
                                @if run.has_changes {
                                    " " span.meta { (run.changed_files) " changed files" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn runs_fragment(data: &RunsData) -> Markup {
    runs_list(&data.project, &data.runs)
}

/// What changes while a run goes on: facts, the stop button, the first
/// message and the output. Everything is element text, never markup.
pub fn run_fragment(data: &RunPageData) -> Markup {
    let run = &data.detail.run;
    let key = data.project.key.as_str();
    html! {
        div #run-live.run-live data-status=(run.status.as_str()) {
            div.row {
                (run_chip(run))
                @if run.status.is_active() {
                    button type="button" data-action="cancel-run" data-run-id=(run.id) {
                        "Cancel run"
                    }
                }
            }
            @if run.status == RunStatus::Queued {
                p.hint.queue-note {
                    "Waiting for a free slot: the agent's limit of concurrent runs or the "
                    "overall limit of runs is reached. It starts by itself, oldest first."
                }
            }
            @if run.status == RunStatus::Interrupted {
                p.hint {
                    "Interrupted: the server stopped or crashed while this run was active, "
                    "so it did not finish. The process may have changed things before that."
                }
            }
            dl.facts {
                dt { "Agent" }
                dd { a href={ "/agents/" (run.agent_id) } { (run.agent) } }
                dt { "Issue" }
                dd { a href={ "/p/" (key) "/" (run.issue_number) } { (run.issue) } }
                dt { "Started because" }
                dd {
                    (run.reason.as_str())
                    @if run.edited { " (message edited by a person)" }
                }
                @if let Some(source) = &run.template_source {
                    dt { "Message written from" }
                    dd { (source.replace('_', " ")) }
                }
                dt { "Created" }
                dd { (run_time("", Some(run.created))) }
                @if let Some(at) = run.queued_at {
                    dt { "Queued" }
                    dd { (run_time("", Some(at))) }
                }
                @if run.started.is_some() {
                    dt { "Started" }
                    dd { (run_time("", run.started)) }
                }
                @if run.finished.is_some() {
                    dt { "Finished" }
                    dd { (run_time("", run.finished)) }
                }
                @if let Some(code) = run.exit_code {
                    dt { "Exit code" }
                    dd { (code) }
                }
                @if let Some(error) = &run.error {
                    dt { "Error" }
                    dd { (error) }
                }
                dt { "Working copy" }
                dd {
                    (worktree_chip(run))
                    @if run.worktree_state == WorktreeState::None {
                        (worktree_text(run))
                    }
                }
                @if let Some(branch) = &run.branch {
                    dt { "Branch" }
                    dd { code.branch { (branch) } }
                }
                @if let Some(path) = &run.worktree_path {
                    dt { "Folder" }
                    dd { code.path { (path) } }
                }
                @if let Some(base) = &run.base_ref {
                    dt { "Started from" }
                    dd {
                        code { (base) }
                        @if let Some(commit) = &run.base_commit {
                            " at " code { (short_sha(commit)) }
                        }
                    }
                }
                @if run.has_changes {
                    dt { "Changes" }
                    dd {
                        (run.changed_files) " changed files, "
                        (run.ahead) " commits ahead"
                    }
                }
                @if let Some(note) = &run.worktree_note {
                    dt { "Working copy note" }
                    dd { (note) }
                }
            }
            h2 { "First message" }
            @if run.status == RunStatus::Queued && data.detail.message.is_empty() {
                p.empty { "It is written when the run starts, from the issue as it is then." }
            } @else {
                pre.run-text { (data.detail.message) }
            }
        }
    }
}

/// The output of a run. The server renders the tail kept on the run row (the
/// whole page works without script); the script then replaces it by the
/// stored transcript and, while the run is active, polls for more
/// (`GET /api/v1/runs/{id}/transcript`). It lives outside the fragment that
/// is swapped as the run changes, so a refresh never resets it. `id` is the
/// panel's element id; `run` and `tail` are absent for the issue page's panel,
/// which the script points at the issue's active run.
fn output_panel(id: &str, run: Option<&Run>, tail: &str) -> Markup {
    let active = run.is_some_and(|r| r.status.is_active());
    let run_id = run.map(|r| r.id);
    html! {
        section.run-output id=(id) aria-labelledby={ (id) "-heading" }
            data-run-id=[run_id] data-active=(active) hidden[run.is_none()] {
            div.output-head {
                h2 id={ (id) "-heading" } { "Output" }
                span.meta data-role="state" role="status" {}
                label.output-follow data-role="follow-label" hidden {
                    input type="checkbox" data-role="follow" checked;
                    " Follow"
                }
                a.output-download data-role="download" href=[run_id.map(|n| format!("/api/v1/runs/{n}/transcript.txt?download=1"))] {
                    "Download"
                }
            }
            @if run.is_some_and(|r| r.output_truncated) {
                p.hint { "Only the last part of the output is shown until the full transcript loads." }
            }
            p.empty data-role="empty" hidden[!tail.is_empty()] {
                @if active {
                    "Nothing yet."
                } @else {
                    "The process printed nothing."
                }
            }
            pre.run-text.transcript data-role="view" tabindex="0" hidden[tail.is_empty()] { (tail) }
            button type="button" data-action="more-output" data-role="more" hidden { "Load more" }
        }
    }
}

pub fn run_page(data: &RunPageData) -> Markup {
    let run = &data.detail.run;
    let key = data.project.key.as_str();
    let title = format!("Run {} of {}", run.id, run.issue);
    let mut frame = Frame::new(&title, "run");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    frame.run_id = Some(run.id);
    layout(
        &frame,
        html! {
            article.page {
                nav.crumbs aria-label="Breadcrumb" {
                    a href={ "/p/" (key) } { (data.project.name) }
                    " / "
                    a href={ "/p/" (key) "/" (run.issue_number) } { (run.issue) }
                    " / "
                    span { "Run " (run.id) }
                }
                div.issue-head {
                    h1 { "Run " (run.id) }
                }
                p.field-msg #page-msg role="alert" {}
                (run_fragment(data))
                (output_panel("run-output", Some(run), &data.detail.output_tail))
            }
        },
    )
}

pub fn issue_page(data: &IssueData) -> Markup {
    let issue = &data.detail.issue;
    let key = data.project.key.as_str();
    let lookup = Lookup::new(
        &data.columns,
        &data.types,
        &data.labels,
        &data.users,
        &data.initiatives,
    );
    let title = format!("{} {}", issue.identifier, issue.title);
    let mut frame = Frame::new(&title, "issue");
    frame.user = Some(&data.user);
    frame.project = Some(&data.project);
    frame.last_event = Some(data.last_event);
    frame.issue_id = Some(issue.id);
    frame.issue_number = Some(issue.number);
    layout(
        &frame,
        html! {
            article.issue {
                nav.crumbs aria-label="Breadcrumb" {
                    a href={ "/p/" (key) } { (data.project.name) }
                    " / "
                    span { (issue.identifier) }
                }
                div.issue-head {
                    h1 { span.ident { (issue.identifier) } }
                    @if issue.archived { span.chip.chip-archived { "archived" } }
                    @if issue.blocked { span.chip.chip-blocked { "Blocked" } }
                    @if issue.archived {
                        button type="button" data-action="unarchive" { "Restore" }
                    } @else {
                        button type="button" data-action="archive" { "Archive" }
                    }
                }
                p.field-msg #page-msg role="alert" {}
                div.issue-grid {
                    div.issue-main {
                        div.field {
                            label for="f-title" { "Title" }
                            input #f-title data-field="title" value=(issue.title) maxlength="300";
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-description" { "Description" }
                            textarea #f-description data-field="description" rows="10" {
                                "\n" (issue.description)
                            }
                            div.row {
                                button type="button" data-action="save-description" { "Save description" }
                            }
                            p.field-msg role="alert" {}
                        }
                        section.related aria-labelledby="family-heading" {
                            h2 #family-heading { "Parent and children" }
                            @if let (Some(parent), Some(title)) = (issue.parent, &data.parent_title) {
                                p {
                                    "Parent: "
                                    a href={ "/p/" (key) "/" (parent) } {
                                        (key) "-" (parent) " " (title)
                                    }
                                }
                            }
                            @if data.detail.children.is_empty() {
                                p.empty { "No child issues." }
                            } @else {
                                ul.children {
                                    @for child in &data.detail.children {
                                        li {
                                            a href={ "/p/" (key) "/" (child.number) } {
                                                (key) "-" (child.number) " " (child.title)
                                            }
                                            " "
                                            span.chip { (lookup.column(child.column_id)) }
                                            @if child.archived { " " span.chip.chip-archived { "archived" } }
                                        }
                                    }
                                }
                            }
                        }
                        (links_section(&data.detail.links, issue.blocked, key, &lookup))
                        (documents_section(&data.detail.documents, key))
                        (runs_list(key, &data.runs))
                        (output_panel("issue-output", None, ""))
                        (comments_list(&data.comments, &data.user))
                        (activity_list(&data.activity, &lookup))
                    }
                    aside.issue-side aria-label="Fields" {
                        div.field {
                            label for="f-column" { "Column" }
                            select #f-column data-field="column" {
                                @for column in &data.columns {
                                    option value=(column.id) selected[column.id == issue.column_id] {
                                        (column.name) " (" (column.state_type.as_str()) ")"
                                    }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-type" { "Type" }
                            select #f-type data-field="type_id" {
                                @for kind in &data.types {
                                    option value=(kind.id) selected[kind.id == issue.type_id] {
                                        (kind.name)
                                    }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-priority" { "Priority" }
                            select #f-priority data-field="priority" {
                                option value="" selected[issue.priority.is_none()] { "None" }
                                @for priority in PRIORITIES {
                                    option value=(priority.as_str())
                                        selected[issue.priority == Some(priority)] {
                                        (priority.as_str())
                                    }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-assignee" { "Assignee" }
                            select #f-assignee data-field="assignee_id" {
                                option value="" selected[issue.assignee_id.is_none()] { "Unassigned" }
                                @for user in data.users.iter().filter(|u| !u.disabled || Some(u.id) == issue.assignee_id) {
                                    option value=(user.id) selected[Some(user.id) == issue.assignee_id] {
                                        (user.name)
                                    }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-initiative" { "Initiative" }
                            select #f-initiative data-field="initiative" {
                                option value="" selected[issue.initiative_id.is_none()] { "None" }
                                @for initiative in &data.initiatives {
                                    option value=(initiative.id)
                                        selected[Some(initiative.id) == issue.initiative_id] {
                                        (initiative.name)
                                        " (" (initiative.status.as_str()) ")"
                                    }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        div.field {
                            label for="f-parent" { "Parent issue number" }
                            input #f-parent data-field="parent" type="number" min="1" inputmode="numeric"
                                value=[issue.parent];
                            p.field-msg role="alert" {}
                        }
                        fieldset.field data-field="labels" {
                            legend { "Labels" }
                            @if data.labels.is_empty() {
                                p.empty { "No labels in this project." }
                            }
                            @for label in &data.labels {
                                label.check {
                                    input type="checkbox" value=(label.id)
                                        checked[issue.labels.contains(&label.id)];
                                    span.chip.chip-label data-color=(label.color) { (label.name) }
                                }
                            }
                            p.field-msg role="alert" {}
                        }
                        p.meta {
                            "Created "
                            time.when datetime=(utc_iso(issue.created)) data-ts=(issue.created) {
                                (utc_text(issue.created))
                            }
                            "."
                        }
                    }
                }
            }
        },
    )
}
