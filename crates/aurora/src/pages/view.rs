//! The HTML of every page, as `maud` templates. `maud` escapes every
//! interpolated value, so user content is never raw HTML; nothing here uses
//! `PreEscaped`. Data for the script goes in `data-*` attributes: the content
//! security policy forbids inline script and style.

use std::collections::HashMap;

use aurora_store::auth::User;
use aurora_store::events::Event;
use aurora_store::issues::{Issue, Priority};
use aurora_store::links::{Blocker, IssueLinks, LinkedIssue};
use aurora_store::projects::{Column, IssueType, Label, Project};
use maud::{DOCTYPE, Markup, html};
use serde_json::Value;

use super::assets;
use super::data::{ActivityData, BoardData, HomeData, IssueData};
use super::time::{utc_iso, utc_text};

const PRIORITIES: [Priority; 4] = [
    Priority::Urgent,
    Priority::High,
    Priority::Medium,
    Priority::Low,
];

pub struct Frame<'a> {
    pub title: &'a str,
    /// `board`, `issue`, `home`, `sign-in` or `setup`; the script picks its behaviour from it.
    pub page: &'static str,
    pub user: Option<&'a User>,
    pub project: Option<&'a Project>,
    pub last_event: Option<i64>,
    pub issue_id: Option<i64>,
    pub issue_number: Option<i64>,
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
                data-issue-number=[frame.issue_number] {
                a.skip href="#main" { "Skip to content" }
                header.topbar {
                    a.brand href="/" { "Aurora" }
                    @if let Some(project) = frame.project {
                        nav.project-nav aria-label="Project" {
                            a.project-link href={ "/p/" (project.key) } {
                                span.key { (project.key) }
                                " "
                                (project.name)
                            }
                            a href="/?list=1" { "All projects" }
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
}

impl<'a> Lookup<'a> {
    fn new(
        columns: &'a [Column],
        types: &'a [IssueType],
        labels: &'a [Label],
        users: &'a [User],
    ) -> Self {
        Lookup {
            columns: columns.iter().map(|c| (c.id, c.name.as_str())).collect(),
            types: types.iter().map(|t| (t.id, t.name.as_str())).collect(),
            labels: labels.iter().map(|l| (l.id, l)).collect(),
            users: users.iter().map(|u| (u.id, u.name.as_str())).collect(),
        }
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
    let lookup = Lookup::new(&data.columns, &data.types, &data.labels, &data.users);
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
                    }
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
    html! {
        section #activity.activity aria-labelledby="activity-heading" {
            h2 #activity-heading { "Activity" }
            @if events.is_empty() {
                p.empty { "Nothing yet." }
            } @else {
                ol.activity-list {
                    @for event in events.iter().rev() {
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

pub fn activity_fragment(data: &ActivityData) -> Markup {
    let lookup = Lookup::new(&data.columns, &data.types, &data.labels, &data.users);
    activity_list(&data.activity, &lookup)
}

pub fn issue_page(data: &IssueData) -> Markup {
    let issue = &data.detail.issue;
    let key = data.project.key.as_str();
    let lookup = Lookup::new(&data.columns, &data.types, &data.labels, &data.users);
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
