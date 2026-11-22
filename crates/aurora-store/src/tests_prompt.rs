//! Prompt templates (TRK-50): the template language, resolution, the context
//! builder, the default template, events and the invariants around them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use serde_json::{Value, json};

use crate::agents::{self, AgentPatch, NewAgent};
use crate::auth::{Principal, Role, Scope};
use crate::comments::{self, NewComment};
use crate::events::{self, EventFilter};
use crate::invariants;
use crate::projects::Rejection;
use crate::prompt::lang::{self, Vars};
use crate::prompt::{
    self, DEFAULT_TEMPLATE, MAX_COMMENT_CHARS, MAX_COMMENTS, MAX_DEPTH, MAX_OUTPUT_CHARS,
    MAX_TEMPLATE_CHARS, PromptRequest, TRUNCATED_SUFFIX, TemplateSource, TriggerCtx, TriggerKind,
    VARIABLES, render_prompt, resolve_template, save_template,
};
use crate::tests_support::documents::{self, DocumentTarget, NewDocument};
use crate::tests_support::issues::{self, MoveTarget, NewIssue};
use crate::tests_support::links::{self, LinkKind};
use crate::tests_support::{projects, test_actor};
use crate::{Store, StoreError};

// ---- the engine, without a database ----

fn vars(pairs: &[(&str, Value)]) -> Vars {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

fn run(template: &str, vars: &Vars) -> String {
    let parsed = lang::parse(template).unwrap_or_else(|err| panic!("{template:?}: {err}"));
    lang::render(&parsed, vars).text
}

fn parse_error(template: &str) -> lang::TemplateError {
    match lang::parse(template) {
        Ok(_) => panic!("{template:?} should not parse"),
        Err(err) => err,
    }
}

#[test]
fn substitution_inserts_text_numbers_and_booleans() {
    let v = vars(&[
        ("issue.title", json!("Fix it")),
        ("issue.number", json!(7)),
        ("trigger.manual", json!(true)),
        ("trigger.assigned", json!(false)),
    ]);
    assert_eq!(
        run(
            "{{issue.title}}|{{issue.number}}|{{trigger.manual}}|{{trigger.assigned}}",
            &v
        ),
        "Fix it|7|true|false"
    );
    // Spaces inside the braces do not matter; a value that is not given is empty.
    assert_eq!(
        run("[{{ issue.title }}][{{project.name}}]", &v),
        "[Fix it][]"
    );
    assert_eq!(run("no tags at all", &v), "no tags at all");
    assert_eq!(run("", &v), "");
}

#[test]
fn if_and_else_follow_the_truthiness_of_each_kind_of_value() {
    let template = "{{#if issue.title}}yes{{else}}no{{/if}}";
    for (value, expected) in [(json!("x"), "yes"), (json!(""), "no"), (Value::Null, "no")] {
        assert_eq!(run(template, &vars(&[("issue.title", value)])), expected);
    }
    let template = "{{#if comments}}some{{else}}none{{/if}}";
    assert_eq!(run(template, &vars(&[("comments", json!([]))])), "none");
    assert_eq!(
        run(template, &vars(&[("comments", json!([{"body": "x"}]))])),
        "some"
    );
    assert_eq!(run(template, &vars(&[])), "none");
    let template = "{{#if comments.omitted}}omitted{{/if}}";
    assert_eq!(run(template, &vars(&[("comments.omitted", json!(0))])), "");
    assert_eq!(
        run(template, &vars(&[("comments.omitted", json!(3))])),
        "omitted"
    );
    let template = "{{#if trigger.manual}}A{{else}}B{{/if}}";
    assert_eq!(
        run(template, &vars(&[("trigger.manual", json!(true))])),
        "A"
    );
    assert_eq!(
        run(template, &vars(&[("trigger.manual", json!(false))])),
        "B"
    );
}

#[test]
fn blocks_nest_up_to_the_depth_limit() {
    let v = vars(&[
        ("issue.title", json!("t")),
        ("issue.labels", json!(["a", "b"])),
        (
            "comments",
            json!([{"author": "ann", "body": "one"}, {"author": "bob", "body": "two"}]),
        ),
    ]);
    assert_eq!(
        run(
            "{{#if issue.title}}{{#if issue.labels}}{{#each issue.labels}}<{{this}}>{{/each}}{{else}}-{{/if}}{{/if}}",
            &v
        ),
        "<a><b>"
    );
    // The innermost loop is `this`.
    assert_eq!(
        run(
            "{{#each comments}}{{this.author}}:{{#each issue.labels}}{{this}}{{/each}};{{/each}}",
            &v
        ),
        "ann:ab;bob:ab;"
    );
    let ok = "{{#if issue.title}}{{#if issue.title}}{{#if issue.title}}{{#if issue.title}}deep{{/if}}{{/if}}{{/if}}{{/if}}";
    assert_eq!(run(ok, &v), "deep");
    assert_eq!(MAX_DEPTH, 4);
    let too_deep = "{{#if issue.title}}{{#if issue.title}}{{#if issue.title}}{{#if issue.title}}{{#if issue.title}}x{{/if}}{{/if}}{{/if}}{{/if}}{{/if}}";
    let err = parse_error(too_deep);
    assert!(err.message.contains("nested more than 4"), "{err}");
    assert_eq!((err.line, err.column), (1, 77), "{err}");
    // Loops count toward the depth as well.
    let mixed = "{{#each comments}}{{#each issue.labels}}{{#if this}}{{#if this}}{{#if this}}x{{/if}}{{/if}}{{/if}}{{/each}}{{/each}}";
    assert!(parse_error(mixed).message.contains("nested more than 4"));
}

#[test]
fn each_gives_this_fields_and_the_position() {
    let v = vars(&[
        ("issue.labels", json!(["bug", "ui"])),
        (
            "issue.children",
            json!([
                {"key": "TRK-2", "title": "first", "state_type": "started"},
                {"key": "TRK-3", "title": "second", "state_type": "backlog"},
            ]),
        ),
    ]);
    assert_eq!(
        run("{{#each issue.labels}}{{@index}}={{this}} {{/each}}", &v),
        "0=bug 1=ui "
    );
    assert_eq!(
        run(
            "{{#each issue.children}}{{this.key}}/{{this.title}}/{{this.state_type}};{{/each}}",
            &v
        ),
        "TRK-2/first/started;TRK-3/second/backlog;"
    );
    assert_eq!(
        run(
            "{{#each issue.labels}}{{#if @index}}, {{/if}}{{this}}{{/each}}",
            &v
        ),
        "bug, ui"
    );
    // An empty or missing list renders nothing.
    assert_eq!(run("a{{#each issue.labels}}x{{/each}}b", &vars(&[])), "ab");
}

#[test]
fn comments_escapes_and_standalone_lines() {
    let v = vars(&[("issue.title", json!("T")), ("trigger.manual", json!(true))]);
    assert_eq!(run("a{{!-- gone --}}b", &v), "ab");
    assert_eq!(run("a{{!-- {{issue.title}} --}}b", &v), "ab");
    assert_eq!(
        run("\\{{issue.title}} is {{issue.title}}", &v),
        "{{issue.title}} is T"
    );
    // A block tag alone on its line takes the whole line with it.
    assert_eq!(
        run("one\n{{#if trigger.manual}}\ntwo\n{{/if}}\nthree\n", &v),
        "one\ntwo\nthree\n"
    );
    assert_eq!(
        run("one\n{{#if issue.labels}}\ntwo\n{{/if}}\nthree\n", &v),
        "one\nthree\n"
    );
    assert_eq!(run("one\n{{!-- note --}}\ntwo", &v), "one\ntwo");
    // Next to text the tag only disappears.
    assert_eq!(run("a {{#if trigger.manual}}b{{/if}} c", &v), "a b c");
    // A plain variable on its own line keeps its line.
    assert_eq!(run("x\n{{issue.title}}\ny", &v), "x\nT\ny");
}

#[test]
fn a_value_is_never_scanned_for_tags() {
    let hostile = "{{issue.title}} {{#if trigger.manual}}pwned{{/if}} {{#each comments}} {{!-- ";
    let v = vars(&[
        ("issue.title", json!("Title")),
        ("issue.description", json!(hostile)),
        ("trigger.manual", json!(true)),
        (
            "comments",
            json!([{"author": "{{issue.title}}", "body": "{{trigger.comment}}"}]),
        ),
    ]);
    assert_eq!(run("[{{issue.description}}]", &v), format!("[{hostile}]"));
    assert_eq!(
        run(
            "{{#each comments}}{{this.author}}|{{this.body}}{{/each}}",
            &v
        ),
        "{{issue.title}}|{{trigger.comment}}"
    );
}

#[test]
fn bad_templates_name_the_problem_and_the_position() {
    let table: &[(&str, &str, usize, usize)] = &[
        (
            "Hello {{issue.titel}}",
            "unknown variable `issue.titel`",
            1,
            7,
        ),
        ("line one\nHello {{issue.titel}}", "unknown variable", 2, 7),
        ("é{{nope}}", "unknown variable `nope`", 1, 2),
        ("{{#if issue.title}}open", "`{{#if}}` is never closed", 1, 1),
        (
            "a\n{{#each comments}}x",
            "`{{#each}}` is never closed",
            2,
            1,
        ),
        ("{{/if}}", "`{{/if}}` without an open", 1, 1),
        ("x {{/each}}", "`{{/each}}` without an open", 1, 3),
        ("{{else}}", "`{{else}}` without an open", 1, 1),
        (
            "{{#if issue.title}}{{/each}}",
            "`{{/each}}` closes a block that was opened with `{{#if}}`",
            1,
            20,
        ),
        (
            "{{#each comments}}{{/if}}",
            "`{{/if}}` closes a block that was opened with `{{#each}}`",
            1,
            19,
        ),
        (
            "{{#if issue.title}}a{{else}}b{{else}}c{{/if}}",
            "only one `{{else}}`",
            1,
            30,
        ),
        (
            "{{#each comments}}{{else}}{{/each}}",
            "`{{else}}` only works inside `{{#if}}`",
            1,
            19,
        ),
        ("{{comments}}", "`comments` is a list", 1, 1),
        ("{{#each issue.title}}x{{/each}}", "not a list", 1, 1),
        (
            "{{#each nothing}}x{{/each}}",
            "unknown variable `nothing`",
            1,
            1,
        ),
        ("{{this}}", "only works inside `{{#each}}`", 1, 1),
        ("{{@index}}", "`@index` only works inside", 1, 1),
        (
            "{{#each comments}}{{this}}{{/each}}",
            "`this` is a record here",
            1,
            19,
        ),
        (
            "{{#each comments}}{{this.nope}}{{/each}}",
            "unknown field `nope`",
            1,
            19,
        ),
        (
            "{{#each issue.labels}}{{this.name}}{{/each}}",
            "items of this list are text",
            1,
            23,
        ),
        ("{{}}", "needs a variable name", 1, 1),
        ("{{ issue title }}", "is not a tag", 1, 1),
        ("{{bad name!}}", "is not a tag", 1, 1),
        ("{{#nope}}", "is not a tag", 1, 1),
        ("{{#if}}x{{/if}}", "needs a variable name", 1, 1),
        ("text {{issue.title", "is never closed", 1, 6),
        ("{{!-- open", "comment is never closed", 1, 1),
        ("\n\n  {{unknown}}", "unknown variable", 3, 3),
    ];
    for (template, message, line, column) in table {
        let err = parse_error(template);
        assert!(
            err.message.contains(message),
            "{template:?}: expected {message:?} in {:?}",
            err.message
        );
        assert_eq!(
            (err.line, err.column),
            (*line, *column),
            "{template:?}: {err}"
        );
        assert!(
            err.to_string()
                .ends_with(&format!("(line {line}, column {column})"))
        );
    }
}

#[test]
fn template_length_limit_counts_characters() {
    let at_limit = "x".repeat(MAX_TEMPLATE_CHARS);
    assert!(lang::parse(&at_limit).is_ok());
    let over = "x".repeat(MAX_TEMPLATE_CHARS + 1);
    let err = parse_error(&over);
    assert!(err.message.contains("20001"), "{err}");
    // Multi-byte characters count once.
    let wide = "é".repeat(MAX_TEMPLATE_CHARS);
    assert!(lang::parse(&wide).is_ok());
    assert!(lang::parse(&format!("{wide}é")).is_err());
}

#[test]
fn unicode_survives_a_render() {
    let v = vars(&[("issue.title", json!("Zażółć gęślą jaźń \u{1F600} 日本語"))]);
    assert_eq!(
        run("→ {{issue.title}} ←", &v),
        "→ Zażółć gęślą jaźń \u{1F600} 日本語 ←"
    );
    let err = parse_error("日本語{{nope}}");
    assert_eq!((err.line, err.column), (1, 4));
}

#[test]
fn a_huge_value_is_shortened_largest_first_and_marked() {
    let big = "d".repeat(3_000_000);
    let small = "c".repeat(5_000);
    let v = vars(&[
        ("issue.title", json!("keep me whole")),
        ("issue.description", json!(big)),
        ("comments", json!([{"body": small.clone()}])),
    ]);
    let parsed = lang::parse(
        "{{issue.title}}|{{issue.description}}|{{#each comments}}{{this.body}}{{/each}}",
    )
    .unwrap();
    let out = lang::render(&parsed, &v);
    assert!(out.truncated);
    let chars = out.text.chars().count();
    assert!(chars <= MAX_OUTPUT_CHARS, "{chars}");
    assert!(out.text.starts_with("keep me whole|"));
    assert!(out.text.contains(TRUNCATED_SUFFIX));
    // The smaller section was not touched; the largest took the cut.
    assert!(out.text.contains(&small));
    assert!(!out.text.ends_with(TRUNCATED_SUFFIX) || out.text.contains(&small));
    let kept = out.text.matches('d').count();
    assert!(kept > 100_000 && kept < 200_000, "{kept}");
}

#[test]
fn several_large_values_are_cut_in_order_of_size() {
    let v = vars(&[
        ("issue.description", json!("A".repeat(150_000))),
        ("project.rules", json!("B".repeat(100_000))),
        ("trigger.comment", json!("C".repeat(1_000))),
    ]);
    let parsed =
        lang::parse("{{issue.description}}\n{{project.rules}}\n{{trigger.comment}}").unwrap();
    let out = lang::render(&parsed, &v);
    assert!(out.truncated);
    assert!(out.text.chars().count() <= MAX_OUTPUT_CHARS);
    let a = out.text.matches('A').count();
    let b = out.text.matches('B').count();
    let c = out.text.matches('C').count();
    assert_eq!(c, 1_000, "the small value is whole");
    assert!(a < 150_000, "the largest was cut: {a}");
    assert!(b <= 100_000);
    assert!(a > 0 && b > 0, "every value keeps a part");
}

#[test]
fn a_text_that_fits_is_not_marked() {
    let v = vars(&[(
        "issue.description",
        json!("a".repeat(MAX_OUTPUT_CHARS - 10)),
    )]);
    let parsed = lang::parse("{{issue.description}}").unwrap();
    let out = lang::render(&parsed, &v);
    assert!(!out.truncated);
    assert_eq!(out.text.chars().count(), MAX_OUTPUT_CHARS - 10);
    let v = vars(&[("issue.description", json!("a".repeat(MAX_OUTPUT_CHARS)))]);
    let out = lang::render(&parsed, &v);
    assert!(!out.truncated, "exactly the limit still fits");
    let v = vars(&[("issue.description", json!("a".repeat(MAX_OUTPUT_CHARS + 1)))]);
    assert!(lang::render(&parsed, &v).truncated);
}

#[test]
fn a_loop_of_loops_is_cut_at_the_limit_instead_of_running_away() {
    let items: Vec<Value> = (0..100)
        .map(|n| json!({"body": format!("{n}").repeat(150), "author": "a"}))
        .collect();
    let v = vars(&[("comments", Value::Array(items))]);
    let parsed = lang::parse(
        "{{#each comments}}{{#each comments}}{{#each comments}}{{this.body}}{{/each}}{{/each}}{{/each}}",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = lang::render(&parsed, &v);
    assert!(out.truncated);
    assert!(out.text.chars().count() <= MAX_OUTPUT_CHARS);
    assert!(
        out.text.ends_with(TRUNCATED_SUFFIX),
        "{}",
        &out.text[out.text.len() - 40..]
    );
    assert!(started.elapsed().as_secs() < 20);

    // Loops over empty bodies produce no text; they must end too.
    let empty: Vec<Value> = (0..100)
        .map(|_| json!({"body": "", "author": ""}))
        .collect();
    let v = vars(&[("comments", Value::Array(empty))]);
    let parsed = lang::parse(
        "{{#each comments}}{{#each comments}}{{#each comments}}{{#each comments}}{{this.body}}{{/each}}{{/each}}{{/each}}{{/each}}",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = lang::render(&parsed, &v);
    assert!(out.truncated);
    assert!(started.elapsed().as_secs() < 20);
}

#[test]
fn variables_used_lists_each_catalogue_name_once_sorted() {
    let parsed = lang::parse(
        "{{issue.title}} {{#if trigger.manual}}{{issue.title}}{{/if}}{{#each comments}}{{this.body}}{{/each}}{{!-- {{x}} --}}",
    )
    .unwrap();
    assert_eq!(
        parsed.variables_used(),
        ["comments", "issue.title", "trigger.manual"]
    );
}

#[test]
fn cutting_text_keeps_the_marker_inside_the_limit() {
    assert_eq!(lang::cut_text("short", 10), None);
    let cut = lang::cut_text(&"x".repeat(100), 50).unwrap();
    assert_eq!(cut.chars().count(), 50);
    assert!(cut.ends_with(TRUNCATED_SUFFIX));
}

// ---- the catalogue ----

#[test]
fn the_catalogue_is_unique_documented_and_usable_in_templates() {
    let mut seen = std::collections::BTreeSet::new();
    for variable in VARIABLES {
        assert!(seen.insert(variable.name), "{} twice", variable.name);
        assert!(!variable.description.is_empty(), "{}", variable.name);
        assert!(!variable.example.is_empty(), "{}", variable.name);
        let template = match variable.kind {
            prompt::catalogue::Kind::Scalar(_) => format!("{{{{{}}}}}", variable.name),
            prompt::catalogue::Kind::List(_) => {
                format!("{{{{#each {0}}}}}x{{{{/each}}}}", variable.name)
            }
        };
        assert!(lang::parse(&template).is_ok(), "{}", variable.name);
        assert!(prompt::catalogue::find(variable.name).is_some());
    }
    assert_eq!(prompt::variable_catalogue().len(), VARIABLES.len());
    assert!(prompt::catalogue::find("issue.nothing").is_none());
}

// ---- a database to render from ----

const NOW: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const HOUR: i64 = 3_600_000;

struct Fx {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
    /// The agent with no instructions (the built-in default applies).
    plain: i64,
    /// An agent with its own instructions.
    scribe: i64,
    task: i64,
    bug: i64,
    item: i64,
}

fn owner() -> Principal {
    Principal {
        user_id: crate::tests_support::TEST_USER_ID,
        role: Role::Owner,
        scope: Scope::Write,
    }
}

fn member() -> Principal {
    Principal {
        user_id: 9_001,
        role: Role::Member,
        scope: Scope::Write,
    }
}

fn agent_principal() -> Principal {
    Principal {
        user_id: 9_002,
        role: Role::Agent,
        scope: Scope::Write,
    }
}

fn base(agent_id: i64, number: i64) -> PromptRequest<'static> {
    PromptRequest {
        agent_id,
        project: "TRK",
        number,
        trigger: TriggerCtx::new(TriggerKind::Manual),
        workdir: None,
        branch: None,
        run_id: None,
        public_url: None,
        override_template: None,
    }
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let ids = store
        .write(|tx| {
            let actor = test_actor(tx);
            projects::create_project(tx, 1, "TRK", "Tracker", None)?;
            let task = projects::create_issue_type(tx, 1, "TRK", "Task")?;
            let bug = projects::create_issue_type(tx, 1, "TRK", "Bug")?;
            projects::create_project(tx, 1, "OTH", "Other", None)?;
            let item = projects::create_issue_type(tx, 1, "OTH", "Item")?;
            let plain = agents::create_agent(
                tx,
                2,
                &actor,
                NewAgent {
                    name: Some("coder".into()),
                    command: "x".into(),
                    projects: vec!["TRK".into(), "OTH".into()],
                    ..NewAgent::default()
                },
            )?;
            let scribe = agents::create_agent(
                tx,
                2,
                &actor,
                NewAgent {
                    name: Some("scribe".into()),
                    command: "x".into(),
                    instructions: Some("Own text for {{issue.key}}.".into()),
                    projects: vec!["TRK".into(), "OTH".into()],
                    ..NewAgent::default()
                },
            )?;
            Ok((plain.id, scribe.id, task.id, bug.id, item.id))
        })
        .await
        .unwrap();
    Fx {
        _dir: dir,
        path,
        store,
        plain: ids.0,
        scribe: ids.1,
        task: ids.2,
        bug: ids.3,
        item: ids.4,
    }
}

impl Fx {
    async fn render(&self, request: PromptRequest<'static>) -> prompt::Rendered {
        self.store
            .read(move |c| render_prompt(c, NOW, &request))
            .await
            .unwrap()
    }

    async fn issue(&self, title: &str, type_id: i64) -> i64 {
        let title = title.to_string();
        self.store
            .write(move |tx| {
                Ok(issues::create_issue(
                    tx,
                    10,
                    "TRK",
                    NewIssue {
                        title,
                        type_id: Some(type_id),
                        ..NewIssue::default()
                    },
                )?
                .issue
                .number)
            })
            .await
            .unwrap()
    }

    async fn comment(&self, number: i64, body: &str, at: i64) -> i64 {
        let body = body.to_string();
        self.store
            .write(move |tx| {
                let actor = test_actor(tx);
                Ok(comments::add_comment(
                    tx,
                    at,
                    &actor,
                    "TRK",
                    number,
                    NewComment { body, parent: None },
                )?
                .id)
            })
            .await
            .unwrap()
    }

    async fn save(
        &self,
        agent: i64,
        project: &'static str,
        issue_type: Option<i64>,
        body: &'static str,
    ) -> prompt::SaveOutcome {
        self.store
            .write(move |tx| {
                save_template(tx, 50, &owner(), agent, Some(project), issue_type, body)
            })
            .await
            .unwrap()
    }

    async fn resolve(
        &self,
        agent: i64,
        project: i64,
        type_id: Option<i64>,
    ) -> (TemplateSource, Option<i64>, String) {
        self.store
            .read(move |c| {
                let r = resolve_template(c, agent, project, type_id)?;
                Ok((r.source, r.template_id, r.body))
            })
            .await
            .unwrap()
    }

    async fn project_id(&self, key: &'static str) -> i64 {
        self.store
            .read(move |c| Ok(crate::projects::get_project(c, key)?.id))
            .await
            .unwrap()
    }

    async fn events(&self, prefix: &'static str) -> Vec<crate::events::Event> {
        self.store
            .read(move |c| {
                events::read_events(
                    c,
                    0,
                    Some(500),
                    &EventFilter {
                        kind_prefix: Some(prefix.to_string()),
                        ..EventFilter::default()
                    },
                )
            })
            .await
            .unwrap()
    }
}

// ---- resolution ----

#[tokio::test]
async fn the_most_specific_template_wins() {
    let fx = fixture().await;
    let trk = fx.project_id("TRK").await;
    let oth = fx.project_id("OTH").await;

    // Nothing stored: the agent's instructions, or the built-in default.
    let (source, id, body) = fx.resolve(fx.plain, trk, Some(fx.task)).await;
    assert_eq!((source, id), (TemplateSource::Builtin, None));
    assert_eq!(body, DEFAULT_TEMPLATE);
    let (source, id, body) = fx.resolve(fx.scribe, trk, Some(fx.task)).await;
    assert_eq!((source, id), (TemplateSource::Agent, None));
    assert_eq!(body, "Own text for {{issue.key}}.");

    // A whitespace-only instruction counts as none.
    fx.store
        .write({
            let scribe = fx.scribe;
            move |tx| {
                tx.execute(
                    "UPDATE agent SET instructions = '  \n ' WHERE id = ?1",
                    [scribe],
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();
    assert_eq!(
        fx.resolve(fx.scribe, trk, None).await.0,
        TemplateSource::Builtin
    );
    fx.store
        .write({
            let scribe = fx.scribe;
            move |tx| {
                tx.execute(
                    "UPDATE agent SET instructions = 'Own text for {{issue.key}}.' WHERE id = ?1",
                    [scribe],
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();

    // (agent, project) beats the instructions.
    let project_level = fx.save(fx.scribe, "TRK", None, "project level").await;
    let (source, id, body) = fx.resolve(fx.scribe, trk, Some(fx.task)).await;
    assert_eq!(
        (source, id, body.as_str()),
        (
            TemplateSource::Project,
            Some(project_level.template.id),
            "project level"
        )
    );
    // The same text for another type and without a type.
    assert_eq!(
        fx.resolve(fx.scribe, trk, Some(fx.bug)).await.0,
        TemplateSource::Project
    );
    assert_eq!(
        fx.resolve(fx.scribe, trk, None).await.0,
        TemplateSource::Project
    );

    // (agent, project, type) beats (agent, project), for that type only.
    let type_level = fx.save(fx.scribe, "TRK", Some(fx.bug), "bug level").await;
    let (source, id, body) = fx.resolve(fx.scribe, trk, Some(fx.bug)).await;
    assert_eq!(
        (source, id, body.as_str()),
        (
            TemplateSource::ProjectType,
            Some(type_level.template.id),
            "bug level"
        )
    );
    assert_eq!(
        fx.resolve(fx.scribe, trk, Some(fx.task)).await.2,
        "project level"
    );

    // Other projects and other agents are not affected.
    let (source, _, body) = fx.resolve(fx.scribe, oth, Some(fx.item)).await;
    assert_eq!(source, TemplateSource::Agent);
    assert_eq!(body, "Own text for {{issue.key}}.");
    assert_eq!(
        fx.resolve(fx.plain, trk, Some(fx.bug)).await.0,
        TemplateSource::Builtin
    );

    // Deleting an override falls back one step.
    fx.store
        .write({
            let (scribe, id) = (fx.scribe, type_level.template.id);
            move |tx| prompt::delete_template(tx, 60, &owner(), scribe, id)
        })
        .await
        .unwrap();
    assert_eq!(
        fx.resolve(fx.scribe, trk, Some(fx.bug)).await.0,
        TemplateSource::Project
    );
    fx.store
        .write({
            let (scribe, id) = (fx.scribe, project_level.template.id);
            move |tx| prompt::delete_template(tx, 61, &owner(), scribe, id)
        })
        .await
        .unwrap();
    assert_eq!(
        fx.resolve(fx.scribe, trk, Some(fx.bug)).await.0,
        TemplateSource::Agent
    );
    fx.store.close().await;
}

#[tokio::test]
async fn an_override_always_names_a_project_that_owns_its_type() {
    let fx = fixture().await;
    let (scribe, task, item) = (fx.scribe, fx.task, fx.item);
    let attempt = |project: Option<&'static str>, type_id: Option<i64>, body: &'static str| {
        let store = fx.store.clone();
        async move {
            store
                .write(move |tx| save_template(tx, 50, &owner(), scribe, project, type_id, body))
                .await
        }
    };
    let err = rejection(attempt(None, Some(task), "x").await.unwrap_err());
    assert!(
        matches!(&err, Rejection::Invalid(m) if m.contains("needs a project")),
        "{err:?}"
    );
    let err = rejection(attempt(None, None, "x").await.unwrap_err());
    assert!(matches!(err, Rejection::Invalid(_)));
    let err = rejection(attempt(Some("OTH"), Some(task), "x").await.unwrap_err());
    assert!(
        matches!(&err, Rejection::Invalid(m) if m.contains("does not belong to project OTH")),
        "{err:?}"
    );
    let err = rejection(attempt(Some("TRK"), Some(item), "x").await.unwrap_err());
    assert!(matches!(err, Rejection::Invalid(_)));
    let err = rejection(attempt(Some("TRK"), Some(987_654), "x").await.unwrap_err());
    assert!(matches!(&err, Rejection::Invalid(m) if m.contains("unknown issue type")));
    let err = rejection(attempt(Some("NOPE"), None, "x").await.unwrap_err());
    assert!(matches!(&err, Rejection::Invalid(m) if m.contains("unknown project")));
    let err = rejection(attempt(Some("TRK"), None, "   \n").await.unwrap_err());
    assert!(matches!(&err, Rejection::Invalid(m) if m.contains("must not be empty")));
    let err = rejection(
        attempt(Some("TRK"), None, "{{issue.titel}}")
            .await
            .unwrap_err(),
    );
    assert!(
        matches!(&err, Rejection::Invalid(m) if m.starts_with("template: unknown variable `issue.titel`") && m.contains("line 1, column 1")),
        "{err:?}"
    );
    assert!(fx.events("prompt_template.").await.is_empty());
    let count: i64 = fx
        .store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM prompt_template", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0);

    let missing_agent = fx
        .store
        .write(|tx| save_template(tx, 50, &owner(), 424_242, Some("TRK"), None, "x"))
        .await
        .unwrap_err();
    assert!(matches!(rejection(missing_agent), Rejection::NotFound(_)));
    fx.store.close().await;
}

#[tokio::test]
async fn saving_the_same_scope_twice_replaces_and_never_duplicates() {
    let fx = fixture().await;
    let first = fx.save(fx.scribe, "TRK", None, "one").await;
    assert!(first.created && first.changed);
    let same = fx.save(fx.scribe, "TRK", None, "one").await;
    assert!(!same.created && !same.changed);
    assert_eq!(same.template.id, first.template.id);
    let replaced = fx.save(fx.scribe, "TRK", None, "two {{issue.key}}").await;
    assert!(!replaced.created && replaced.changed);
    assert_eq!(replaced.template.id, first.template.id);
    assert_eq!(replaced.template.body, "two {{issue.key}}");
    assert_eq!(replaced.template.updated, 50);

    // The type-level row is another row; so is another agent's.
    let typed = fx.save(fx.scribe, "TRK", Some(fx.task), "typed").await;
    assert!(typed.created);
    let typed_again = fx.save(fx.scribe, "TRK", Some(fx.task), "typed 2").await;
    assert!(!typed_again.created);
    assert_eq!(typed_again.template.id, typed.template.id);
    let other_agent = fx.save(fx.plain, "TRK", None, "plain's").await;
    assert!(other_agent.created);
    assert_ne!(other_agent.template.id, first.template.id);

    // The database itself refuses a second row for one scope, with or
    // without an issue type (NULLs would otherwise all count as different).
    let (scribe, trk, task) = (fx.scribe, fx.project_id("TRK").await, fx.task);
    let duplicate = fx
        .store
        .write(move |tx| {
            Ok(tx.execute(
                "INSERT INTO prompt_template (agent_id, project_id, issue_type_id, body, created, updated)
                 VALUES (?1, ?2, NULL, 'dup', 1, 1)",
                rusqlite::params![scribe, trk],
            ))
        })
        .await
        .unwrap();
    assert!(duplicate.is_err(), "a second NULL-type row must be refused");
    let duplicate = fx
        .store
        .write(move |tx| {
            Ok(tx.execute(
                "INSERT INTO prompt_template (agent_id, project_id, issue_type_id, body, created, updated)
                 VALUES (?1, ?2, ?3, 'dup', 1, 1)",
                rusqlite::params![scribe, trk, task],
            ))
        })
        .await
        .unwrap();
    assert!(duplicate.is_err());

    let listed = fx
        .store
        .read(move |c| prompt::list_templates(c, &owner(), scribe))
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].issue_type_id, None, "the project-wide one first");
    assert_eq!(listed[1].issue_type.as_deref(), Some("Task"));
    assert_eq!(listed[0].project, "TRK");
    fx.store.close().await;
}

#[tokio::test]
async fn only_the_owner_manages_templates() {
    let fx = fixture().await;
    let saved = fx.save(fx.scribe, "TRK", None, "secret text").await;
    let scribe = fx.scribe;
    for who in [member(), agent_principal()] {
        let err = fx
            .store
            .read(move |c| prompt::list_templates(c, &who, scribe))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
        let err = fx
            .store
            .write(move |tx| save_template(tx, 70, &who, scribe, Some("TRK"), None, "x"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
        let id = saved.template.id;
        let err = fx
            .store
            .write(move |tx| prompt::delete_template(tx, 70, &who, scribe, id))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
    }
    let listed = fx
        .store
        .read(move |c| prompt::list_templates(c, &owner(), scribe))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].body, "secret text");

    // Unknown agent and unknown or foreign template ids are not found.
    let err = fx
        .store
        .read(|c| prompt::list_templates(c, &owner(), 777))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::NotFound(_)));
    let (plain, id) = (fx.plain, saved.template.id);
    let err = fx
        .store
        .write(move |tx| prompt::delete_template(tx, 70, &owner(), plain, id))
        .await
        .unwrap_err();
    assert!(
        matches!(rejection(err), Rejection::NotFound(_)),
        "another agent's template"
    );
    let err = fx
        .store
        .write(move |tx| prompt::delete_template(tx, 70, &owner(), scribe, 31_337))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::NotFound(_)));
    fx.store.close().await;
}

#[tokio::test]
async fn an_issue_type_with_overrides_cannot_be_deleted() {
    let fx = fixture().await;
    fx.save(fx.scribe, "TRK", Some(fx.bug), "bug level").await;
    let bug = fx.bug;
    let err = fx
        .store
        .write(move |tx| projects::delete_issue_type(tx, 80, "TRK", bug))
        .await
        .unwrap_err();
    assert!(
        matches!(&rejection(err), Rejection::Conflict(m) if m.contains("prompt template")),
        "the type is in use"
    );
    // Without the override it can go.
    let (scribe, id) = (fx.scribe, {
        let scribe = fx.scribe;
        fx.store
            .read(move |c| prompt::list_templates(c, &owner(), scribe))
            .await
            .unwrap()[0]
            .id
    });
    fx.store
        .write(move |tx| prompt::delete_template(tx, 81, &owner(), scribe, id))
        .await
        .unwrap();
    fx.store
        .write(move |tx| projects::delete_issue_type(tx, 82, "TRK", bug))
        .await
        .unwrap();
    fx.store.close().await;
}

#[tokio::test]
async fn instructions_are_validated_as_templates() {
    let fx = fixture().await;
    let created = fx
        .store
        .write(|tx| {
            agents::create_agent(
                tx,
                90,
                &owner(),
                NewAgent {
                    name: Some("broken".into()),
                    command: "x".into(),
                    instructions: Some("Hello {{issue.titel}}".into()),
                    ..NewAgent::default()
                },
            )
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&rejection(created), Rejection::Invalid(m) if m.starts_with("instructions: unknown variable") && m.contains("(line 1, column 7)"))
    );
    let scribe = fx.scribe;
    let updated = fx
        .store
        .write(move |tx| {
            agents::update_agent(
                tx,
                91,
                &owner(),
                scribe,
                AgentPatch {
                    instructions: Some("{{#if issue.title}}".into()),
                    ..AgentPatch::default()
                },
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(updated), Rejection::Invalid(_)));
    // Plain text with no tags at all is still fine, and so is clearing.
    fx.store
        .write(move |tx| {
            agents::update_agent(
                tx,
                92,
                &owner(),
                scribe,
                AgentPatch {
                    instructions: Some("Just do it.".into()),
                    ..AgentPatch::default()
                },
            )
        })
        .await
        .unwrap();
    fx.store
        .write(move |tx| {
            agents::update_agent(
                tx,
                93,
                &owner(),
                scribe,
                AgentPatch {
                    instructions: Some(String::new()),
                    ..AgentPatch::default()
                },
            )
        })
        .await
        .unwrap();
    fx.store.close().await;
}

// ---- the context ----

#[tokio::test]
async fn the_context_names_every_catalogue_variable_and_nothing_else() {
    let fx = fixture().await;
    let n = fx.issue("Main", fx.task).await;
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &base(1, n)))
        .await
        .unwrap();
    let produced: Vec<&str> = context.vars.keys().map(String::as_str).collect();
    let mut expected: Vec<&str> = VARIABLES.iter().map(|v| v.name).collect();
    expected.sort_unstable();
    assert_eq!(produced, expected);
    fx.store.close().await;
}

#[tokio::test]
async fn context_lists_comments_oldest_first_and_marks_the_trigger() {
    let fx = fixture().await;
    let n = fx.issue("Main", fx.task).await;
    let first = fx.comment(n, "first", 1_000).await;
    let second = fx.comment(n, "second, mentions the agent", 2_000).await;
    let third = fx.comment(n, "third", 3_000).await;
    fx.store
        .write(move |tx| comments::delete_comment(tx, 4_000, &test_actor(tx), "TRK", first))
        .await
        .unwrap();

    let plain = fx.plain;
    let request = PromptRequest {
        trigger: TriggerCtx {
            kind: TriggerKind::Mentioned,
            comment_id: Some(second),
            actor_id: None,
        },
        ..base(plain, n)
    };
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &request))
        .await
        .unwrap();
    let comments = context.vars["comments"].as_array().unwrap();
    let bodies: Vec<&str> = comments
        .iter()
        .map(|c| c["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        bodies,
        ["second, mentions the agent", "third"],
        "deleted ones are left out"
    );
    assert_eq!(comments[0]["is_trigger"], true);
    assert_eq!(comments[1]["is_trigger"], false);
    assert_eq!(comments[0]["author"], "test actor");
    assert_eq!(comments[0]["kind"], "human");
    assert_eq!(comments[0]["created"], "1970-01-01T00:00:02Z");
    assert_eq!(context.vars["comments.omitted"], 0);
    assert_eq!(
        context.vars["trigger.comment"],
        "second, mentions the agent"
    );
    assert_eq!(context.vars["trigger.actor"], "test actor");
    assert_eq!(context.vars["trigger"], "mentioned");
    assert_eq!(context.vars["trigger.mentioned"], true);
    assert_eq!(context.vars["trigger.assigned"], false);
    assert!(!context.truncated);
    let _ = third;

    // A trigger comment that was deleted since leaves no text and no mark.
    let request = PromptRequest {
        trigger: TriggerCtx {
            kind: TriggerKind::Mentioned,
            comment_id: Some(first),
            actor_id: None,
        },
        ..base(plain, n)
    };
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &request))
        .await
        .unwrap();
    assert_eq!(context.vars["trigger.comment"], "");
    assert!(
        context.vars["comments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["is_trigger"] == false)
    );

    // An explicit actor wins over the comment's author.
    let request = PromptRequest {
        trigger: TriggerCtx {
            kind: TriggerKind::Manual,
            comment_id: None,
            actor_id: Some(crate::tests_support::TEST_USER_ID),
        },
        ..base(plain, n)
    };
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &request))
        .await
        .unwrap();
    assert_eq!(context.vars["trigger.actor"], "test actor");

    // A comment of another issue is refused.
    let other = fx.issue("Other", fx.task).await;
    let request = PromptRequest {
        trigger: TriggerCtx {
            kind: TriggerKind::Mentioned,
            comment_id: Some(second),
            actor_id: None,
        },
        ..base(plain, other)
    };
    let err = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &request))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    fx.store.close().await;
}

#[tokio::test]
async fn context_keeps_the_last_thirty_comments_and_counts_the_rest() {
    let fx = fixture().await;
    let n = fx.issue("Busy", fx.task).await;
    for i in 0..(MAX_COMMENTS + 7) {
        fx.comment(n, &format!("comment {i}"), 1_000 + i * 10).await;
    }
    let long = "z".repeat(MAX_COMMENT_CHARS + 500);
    fx.comment(n, &long, 999_999).await;
    let plain = fx.plain;
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &base(plain, n)))
        .await
        .unwrap();
    let shown = context.vars["comments"].as_array().unwrap();
    assert_eq!(shown.len() as i64, MAX_COMMENTS);
    assert_eq!(context.vars["comments.omitted"], 8);
    assert_eq!(shown[0]["body"], "comment 8");
    let last = shown[shown.len() - 1]["body"].as_str().unwrap();
    assert_eq!(last.chars().count(), MAX_COMMENT_CHARS);
    assert!(last.ends_with(TRUNCATED_SUFFIX));
    assert!(context.truncated);
    fx.store.close().await;
}

#[tokio::test]
async fn context_shows_only_open_blockers_but_all_links() {
    let fx = fixture().await;
    let main = fx.issue("Main", fx.task).await;
    let open = fx.issue("Open blocker", fx.task).await;
    let done = fx.issue("Done blocker", fx.task).await;
    let blocked = fx.issue("Blocked by main", fx.task).await;
    let related = fx.issue("Related", fx.task).await;
    fx.store
        .write(move |tx| {
            links::add_link(tx, 20, "TRK", open, main, LinkKind::Blocks)?;
            links::add_link(tx, 20, "TRK", done, main, LinkKind::Blocks)?;
            links::add_link(tx, 20, "TRK", main, blocked, LinkKind::Blocks)?;
            links::add_link(tx, 20, "TRK", main, related, LinkKind::Relates)?;
            let column =
                projects::columns_by_state_type(tx, "TRK", projects::StateType::Completed)?
                    .remove(0);
            issues::move_issue(
                tx,
                21,
                "TRK",
                done,
                MoveTarget {
                    column_id: column.id,
                    ..MoveTarget::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let plain = fx.plain;
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &base(plain, main)))
        .await
        .unwrap();
    let keys = |name: &str| -> Vec<String> {
        context.vars[name]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(keys("blockers"), [format!("TRK-{open}")]);
    let mut all = keys("links.blocked_by");
    all.sort();
    let mut both = vec![format!("TRK-{open}"), format!("TRK-{done}")];
    both.sort();
    assert_eq!(all, both);
    assert_eq!(keys("links.blocks"), [format!("TRK-{blocked}")]);
    assert_eq!(keys("links.related"), [format!("TRK-{related}")]);
    let blocker = &context.vars["blockers"][0];
    assert_eq!(blocker["title"], "Open blocker");
    assert_eq!(blocker["state_type"], "backlog");
    fx.store.close().await;
}

#[tokio::test]
async fn context_describes_the_issue_its_children_labels_documents_and_url() {
    let fx = fixture().await;
    let (main, child) = fx
        .store
        .write(|tx| {
            let actor = test_actor(tx);
            let label = projects::create_label(tx, 1, "TRK", "ui", "#00ff00")?;
            let other_label = projects::create_label(tx, 1, "TRK", "bug", "#ff0000")?;
            let main = issues::create_issue(
                tx,
                10,
                "TRK",
                NewIssue {
                    title: "Main".into(),
                    description: Some("Describe {{issue.title}} here".into()),
                    type_id: Some(1),
                    assignee_id: Some(actor.user_id),
                    priority: Some(crate::issues::Priority::High),
                    labels: vec![label.id, other_label.id],
                    ..NewIssue::default()
                },
            )?;
            let child = issues::create_issue(
                tx,
                11,
                "TRK",
                NewIssue {
                    title: "Child".into(),
                    type_id: Some(1),
                    parent: Some(main.issue.number),
                    ..NewIssue::default()
                },
            )?;
            let doc = documents::create_document(
                tx,
                12,
                "TRK",
                NewDocument {
                    title: "Handbook".into(),
                    body: Some("text".into()),
                    parent: None,
                },
            )?;
            documents::add_link(
                tx,
                13,
                "TRK",
                doc.document.id,
                DocumentTarget::Issue(main.issue.number),
            )?;
            Ok((main.issue.number, child.issue.number))
        })
        .await
        .unwrap();
    let plain = fx.plain;
    let request = PromptRequest {
        workdir: Some("C:/work/tree"),
        branch: Some("aurora/TRK-1-run41"),
        run_id: Some(41),
        public_url: Some("http://aurora.example:8080/"),
        ..base(plain, main)
    };
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &request))
        .await
        .unwrap();
    let v = &context.vars;
    assert_eq!(v["issue.key"], format!("TRK-{main}"));
    assert_eq!(v["issue.number"], main);
    assert_eq!(v["issue.title"], "Main");
    assert_eq!(v["issue.description"], "Describe {{issue.title}} here");
    assert_eq!(v["issue.type"], "Task");
    assert_eq!(v["issue.column"], "Backlog");
    assert_eq!(v["issue.state_type"], "backlog");
    assert_eq!(v["issue.priority"], "high");
    assert_eq!(v["issue.assignee"], "test actor");
    assert_eq!(v["issue.labels"], json!(["bug", "ui"]));
    assert_eq!(v["issue.parent"], "");
    assert_eq!(v["issue.children"][0]["key"], format!("TRK-{child}"));
    assert_eq!(v["issue.children"][0]["title"], "Child");
    assert_eq!(
        v["issue.url"],
        format!("http://aurora.example:8080/p/TRK/{main}")
    );
    assert_eq!(v["project.key"], "TRK");
    assert_eq!(v["project.name"], "Tracker");
    assert_eq!(v["project.rules"], "");
    assert_eq!(v["documents"][0]["title"], "Handbook");
    assert_eq!(v["agent.name"], "coder");
    assert_eq!(v["workdir"], "C:/work/tree");
    assert_eq!(v["run.id"], "41");
    assert_eq!(v["run.branch"], "aurora/TRK-1-run41");
    assert_eq!(v["server.url"], "http://aurora.example:8080/mcp");
    assert_eq!(v["now"], "2026-01-01T00:00:00Z");
    assert_eq!(v["trigger.manual"], true);

    // The child knows its parent; without a public URL links are relative.
    let request = base(plain, child);
    let context = fx
        .store
        .read(move |c| prompt::context_for(c, NOW, &request))
        .await
        .unwrap();
    assert_eq!(context.vars["issue.parent"], format!("TRK-{main}"));
    assert_eq!(context.vars["issue.url"], format!("/p/TRK/{child}"));
    assert_eq!(context.vars["server.url"], "/mcp");
    assert_eq!(context.vars["workdir"], "");
    assert_eq!(context.vars["run.id"], "");
    assert_eq!(context.vars["run.branch"], "");

    // Unknown agent, project and issue are not found.
    for request in [
        base(31_337, main),
        PromptRequest {
            project: "NOPE",
            ..base(plain, main)
        },
        base(plain, 9_999),
    ] {
        let err = fx
            .store
            .read(move |c| prompt::context_for(c, NOW, &request))
            .await
            .unwrap_err();
        assert!(
            matches!(rejection(err), Rejection::NotFound(_)),
            "{request:?}"
        );
    }
    fx.store.close().await;
}

#[tokio::test]
async fn now_comes_from_the_caller() {
    let fx = fixture().await;
    let n = fx.issue("Clock", fx.task).await;
    let plain = fx.plain;
    let render_at = |at: i64| {
        let store = fx.store.clone();
        async move {
            store
                .read(move |c| {
                    render_prompt(
                        c,
                        at,
                        &PromptRequest {
                            override_template: Some("{{now}}"),
                            ..base(plain, n)
                        },
                    )
                })
                .await
                .unwrap()
                .text
        }
    };
    assert_eq!(render_at(NOW).await, "2026-01-01T00:00:00Z");
    assert_eq!(
        render_at(NOW + 5 * HOUR + 61_000).await,
        "2026-01-01T05:01:01Z"
    );
    assert_eq!(render_at(0).await, "1970-01-01T00:00:00Z");
    assert_eq!(render_at(951_782_400_000).await, "2000-02-29T00:00:00Z");
    assert_eq!(prompt::iso8601(-1_000), "1969-12-31T23:59:59Z");
    fx.store.close().await;
}

/// Counts the statements `work` runs on a separate connection.
async fn statements<T: Send + 'static>(
    path: PathBuf,
    work: impl FnOnce(&Connection) -> T + Send + 'static,
) -> (T, usize) {
    tokio::task::spawn_blocking(move || {
        let mut conn = Connection::open(path).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        conn.trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(move |event: TraceEvent<'_>| {
                if matches!(event, TraceEvent::Stmt(..)) {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }),
        )
        .unwrap();
        let out = work(&conn);
        (out, count.load(Ordering::SeqCst))
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn building_the_context_runs_a_fixed_number_of_statements() {
    let fx = fixture().await;
    let main = fx.issue("Main", fx.task).await;
    let one_of_each = |fx: &Fx, main: i64, extra: usize| {
        let store = fx.store.clone();
        let task = fx.task;
        async move {
            for i in 0..extra {
                let blocker = store
                    .write(move |tx| {
                        Ok(issues::create_issue(
                            tx,
                            30,
                            "TRK",
                            NewIssue {
                                title: format!("peer {i}"),
                                type_id: Some(task),
                                parent: Some(main),
                                ..NewIssue::default()
                            },
                        )?
                        .issue
                        .number)
                    })
                    .await
                    .unwrap();
                store
                    .write(move |tx| {
                        links::add_link(tx, 31, "TRK", blocker, main, LinkKind::Blocks)?;
                        let doc = documents::create_document(
                            tx,
                            32,
                            "TRK",
                            NewDocument {
                                title: format!("doc {i}"),
                                body: None,
                                parent: None,
                            },
                        )?;
                        documents::add_link(
                            tx,
                            33,
                            "TRK",
                            doc.document.id,
                            DocumentTarget::Issue(main),
                        )?;
                        let actor = test_actor(tx);
                        comments::add_comment(
                            tx,
                            34 + i as i64,
                            &actor,
                            "TRK",
                            main,
                            NewComment {
                                body: format!("c {i}"),
                                parent: None,
                            },
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
        }
    };
    let plain = fx.plain;
    one_of_each(&fx, main, 2).await;
    let (few, few_statements) = statements(fx.path.clone(), move |c| {
        render_prompt(c, NOW, &base(plain, main)).unwrap()
    })
    .await;
    one_of_each(&fx, main, 40).await;
    let (many, many_statements) = statements(fx.path.clone(), move |c| {
        render_prompt(c, NOW, &base(plain, main)).unwrap()
    })
    .await;
    assert!(few.text.len() < many.text.len());
    assert!(few_statements > 0);
    assert_eq!(
        few_statements, many_statements,
        "the number of queries must not depend on the number of comments, links or sub-issues"
    );
    assert!(many_statements < 30, "{many_statements}");
    fx.store.close().await;
}

#[tokio::test]
async fn rendering_changes_nothing() {
    let fx = fixture().await;
    let n = fx.issue("Read only", fx.task).await;
    let plain = fx.plain;
    let before = fx
        .store
        .read(|c| {
            Ok((
                events::latest_event_id(c)?,
                c.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))?,
            ))
        })
        .await
        .unwrap();
    for _ in 0..3 {
        fx.render(base(plain, n)).await;
    }
    let after = fx
        .store
        .read(|c| {
            Ok((
                events::latest_event_id(c)?,
                c.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(before.0, after.0);
    fx.store.close().await;
}

// ---- render_prompt ----

#[tokio::test]
async fn render_prompt_reports_its_source_and_variables() {
    let fx = fixture().await;
    let n = fx.issue("Rendered", fx.task).await;
    let (plain, scribe) = (fx.plain, fx.scribe);

    let built_in = fx.render(base(plain, n)).await;
    assert_eq!(built_in.template_source, TemplateSource::Builtin);
    assert_eq!(built_in.template_id, None);
    assert_eq!(built_in.chars, built_in.text.chars().count());
    assert!(built_in.variables_used.contains(&"issue.key".to_string()));
    assert!(!built_in.truncated);

    let own = fx.render(base(scribe, n)).await;
    assert_eq!(own.template_source, TemplateSource::Agent);
    assert_eq!(own.text, format!("Own text for TRK-{n}."));
    assert_eq!(own.variables_used, ["issue.key"]);

    let saved = fx
        .save(scribe, "TRK", Some(fx.task), "Typed: {{issue.title}}")
        .await;
    let typed = fx.render(base(scribe, n)).await;
    assert_eq!(typed.text, "Typed: Rendered");
    assert_eq!(typed.template_source, TemplateSource::ProjectType);
    assert_eq!(typed.template_id, Some(saved.template.id));

    // Text given with the request replaces whatever is stored.
    let text = "By hand for {{issue.key}} in {{project.key}}".to_string();
    let inline = fx
        .store
        .read(move |c| {
            render_prompt(
                c,
                NOW,
                &PromptRequest {
                    override_template: Some(&text),
                    ..base(scribe, n)
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(inline.text, format!("By hand for TRK-{n} in TRK"));
    assert_eq!(inline.template_source, TemplateSource::Inline);
    assert_eq!(inline.template_id, None);
    assert_eq!(inline.variables_used, ["issue.key", "project.key"]);

    // A broken text is an error that says where it is, not a half message.
    let broken = "ok\n{{issue.nope}}".to_string();
    let err = fx
        .store
        .read(move |c| {
            render_prompt(
                c,
                NOW,
                &PromptRequest {
                    override_template: Some(&broken),
                    ..base(scribe, n)
                },
            )
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&rejection(err), Rejection::Invalid(m) if m.starts_with("the inline template: unknown variable") && m.contains("line 2, column 1"))
    );
    fx.store.close().await;
}

#[tokio::test]
async fn a_huge_description_and_comments_stay_within_the_limits() {
    let fx = fixture().await;
    let n = fx
        .store
        .write(|tx| {
            Ok(issues::create_issue(
                tx,
                10,
                "TRK",
                NewIssue {
                    title: "Huge".into(),
                    description: Some("w".repeat(crate::issues::MAX_DESCRIPTION_CHARS)),
                    type_id: Some(1),
                    ..NewIssue::default()
                },
            )?
            .issue
            .number)
        })
        .await
        .unwrap();
    for i in 0..40 {
        fx.comment(n, &"y".repeat(20_000), 100 + i).await;
    }
    let plain = fx.plain;
    let template = "{{issue.description}}\n{{#each comments}}{{this.body}}\n{{/each}}\n{{#each comments}}{{this.body}}\n{{/each}}"
        .to_string();
    let rendered = fx
        .store
        .read(move |c| {
            render_prompt(
                c,
                NOW,
                &PromptRequest {
                    override_template: Some(&template),
                    ..base(plain, n)
                },
            )
        })
        .await
        .unwrap();
    assert!(rendered.truncated);
    assert!(rendered.chars <= MAX_OUTPUT_CHARS);
    assert!(rendered.text.contains(TRUNCATED_SUFFIX));
    fx.store.close().await;
}

// ---- the default template ----

async fn golden_fixture() -> (Fx, i64, i64) {
    let fx = fixture().await;
    let minimal = fx.issue("Bare issue", fx.task).await;
    let full = fx.issue("Full issue", fx.task).await;
    let blocker = fx.issue("Needs doing first", fx.task).await;
    let blocked = fx.issue("Waits for full", fx.task).await;
    let related = fx.issue("Nearby", fx.task).await;
    let child = fx.issue("Sub task", fx.task).await;
    let mention = {
        fx.comment(full, "Earlier note", 1_000).await;
        fx.comment(full, "@coder please take this", 2_000).await
    };
    let _ = mention;
    fx.store
        .write(move |tx| {
            let actor = test_actor(tx);
            let label = projects::create_label(tx, 5, "TRK", "ui", "#00ff00")?;
            issues::update_issue(
                tx,
                6,
                "TRK",
                full,
                issues::IssuePatch {
                    description: Some(Some("Make the board\nscroll.".into())),
                    priority: Some(Some(crate::issues::Priority::High)),
                    labels: Some(vec![label.id]),
                    assignee_id: Some(Some(actor.user_id)),
                    ..issues::IssuePatch::default()
                },
            )?;
            issues::update_issue(
                tx,
                7,
                "TRK",
                child,
                issues::IssuePatch {
                    parent: Some(Some(full)),
                    ..issues::IssuePatch::default()
                },
            )?;
            links::add_link(tx, 8, "TRK", blocker, full, LinkKind::Blocks)?;
            links::add_link(tx, 8, "TRK", full, blocked, LinkKind::Blocks)?;
            links::add_link(tx, 8, "TRK", full, related, LinkKind::Relates)?;
            let doc = documents::create_document(
                tx,
                9,
                "TRK",
                NewDocument {
                    title: "Design note".into(),
                    body: None,
                    parent: None,
                },
            )?;
            documents::add_link(tx, 9, "TRK", doc.document.id, DocumentTarget::Issue(full))?;
            projects::update_project(
                tx,
                9,
                &actor,
                "TRK",
                projects::ProjectPatch {
                    rules: Some(Some("Keep it small.".into())),
                    ..projects::ProjectPatch::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (fx, minimal, full)
}

#[tokio::test]
async fn golden_default_template_with_every_optional_part_absent() {
    let (fx, minimal, _) = golden_fixture().await;
    // The project rules are a project-level fact; clear them for this case.
    fx.store
        .write(|tx| {
            projects::update_project(
                tx,
                10,
                &test_actor(tx),
                "TRK",
                projects::ProjectPatch {
                    rules: Some(None),
                    ..projects::ProjectPatch::default()
                },
            )
        })
        .await
        .unwrap();
    let plain = fx.plain;
    let rendered = fx.render(base(plain, minimal)).await;
    assert_eq!(rendered.template_source, TemplateSource::Builtin);
    assert!(!rendered.truncated);
    assert_eq!(rendered.text, golden_absent(minimal));
    fx.store.close().await;
}

#[tokio::test]
async fn golden_default_template_with_every_optional_part_present() {
    let (fx, _, full) = golden_fixture().await;
    let plain = fx.plain;
    let request = PromptRequest {
        trigger: TriggerCtx {
            kind: TriggerKind::Mentioned,
            comment_id: None,
            actor_id: Some(crate::tests_support::TEST_USER_ID),
        },
        workdir: Some("C:/work/tree"),
        run_id: Some(7),
        public_url: Some("http://aurora.test:7878"),
        ..base(plain, full)
    };
    let comment_id = fx
        .store
        .read(move |c| {
            Ok(c.query_row(
                "SELECT id FROM comment WHERE issue_id = (SELECT id FROM issue WHERE number = ?1 AND project_id = 1) AND body LIKE '@coder%'",
                [full],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    let request = PromptRequest {
        trigger: TriggerCtx {
            comment_id: Some(comment_id),
            ..request.trigger
        },
        ..request
    };
    let rendered = fx.render(request).await;
    assert!(!rendered.truncated);
    assert_eq!(rendered.text, golden_present(full));
    fx.store.close().await;
}

#[tokio::test]
async fn the_default_template_words_each_trigger_differently() {
    let (fx, minimal, _) = golden_fixture().await;
    let plain = fx.plain;
    let actor = Some(crate::tests_support::TEST_USER_ID);
    let line = |kind: TriggerKind, actor_id: Option<i64>| {
        let request = PromptRequest {
            trigger: TriggerCtx {
                kind,
                comment_id: None,
                actor_id,
            },
            ..base(plain, minimal)
        };
        let fx = &fx;
        async move {
            let text = fx.render(request).await.text;
            text.lines().nth(2).unwrap().to_string()
        }
    };
    assert_eq!(
        line(TriggerKind::Assigned, actor).await,
        "You were started because the issue was assigned to you by test actor."
    );
    assert_eq!(
        line(TriggerKind::Assigned, None).await,
        "You were started because the issue was assigned to you."
    );
    assert_eq!(
        line(TriggerKind::Schedule, None).await,
        "You were started by a schedule."
    );
    assert_eq!(
        line(TriggerKind::Manual, actor).await,
        "A person started you by hand (test actor)."
    );
    assert_eq!(
        line(TriggerKind::Mentioned, actor).await,
        "You were started because you were mentioned in a comment by test actor:"
    );
    fx.store.close().await;
}

#[test]
fn the_default_template_is_valid_and_uses_only_known_variables() {
    let parsed = prompt::templates::builtin_template().unwrap();
    for name in parsed.variables_used() {
        assert!(prompt::catalogue::find(&name).is_some(), "{name}");
    }
    assert!(DEFAULT_TEMPLATE.lines().count() < 110);
    assert!(DEFAULT_TEMPLATE.chars().count() < MAX_TEMPLATE_CHARS);
}

/// The closing rules of the default template; only the MCP address differs.
fn how_to_work(mcp: &str) -> String {
    format!(
        "## How to work
1. Read the task and its comments above before you change anything. Use the Aurora MCP tools ({mcp}): get_issue, list_comments, update_issue, move_issue, add_comment and the others.
2. Find columns by their state type, not by their name. Before you start, move this issue to a column of state type \"started\". When the Done-when conditions are met, move it to a column of state type \"completed\".
3. Add a comment only for what is left undone or for a decision the next person needs: one short handoff comment. It is not a diary.
4. Never commit, push or change git history unless the task explicitly asks you to.
5. Do not touch other issues than this one, except to link or comment where the task requires it.
6. If something is unclear or too big for one session, say so in a comment on this issue instead of guessing.
7. If the result is something to look at or try by hand, a person confirms it: say exactly what to look at and leave the card for confirmation.
8. When you have finished, stop.
"
    )
}

fn golden_absent(_number: i64) -> String {
    format!(
        "You are coder. Your task is issue TRK-1 \"Bare issue\" in project Tracker (TRK).
Issue page: /p/TRK/1
A person started you by hand.

## The task
Type: Task. Column: Backlog (backlog).

(The issue has no description.)

{}",
        how_to_work("/mcp")
    )
}

fn golden_present(_number: i64) -> String {
    format!(
        "You are coder. Your task is issue TRK-2 \"Full issue\" in project Tracker (TRK).
Issue page: http://aurora.test:7878/p/TRK/2
You were started because you were mentioned in a comment by test actor:
@coder please take this
Your working copy is C:/work/tree

## The task
Type: Task. Column: Backlog (backlog). Priority: high. Assignee: test actor.
Labels: ui

Make the board
scroll.

## Sub-issues
- TRK-6 Sub task (backlog)

## Recent comments, oldest first
[1970-01-01T00:00:01Z] test actor (human)
Earlier note

[1970-01-01T00:00:02Z] test actor (human) <- this comment started you
@coder please take this


## Open blockers
These issues block this one and are not finished:
- TRK-3 Needs doing first (backlog)
Before you start, check whether they really prevent the work. If they do, say so in a comment on this issue and stop. If you are not sure, ask in a comment instead of guessing.

## This issue blocks
- TRK-4 Waits for full (backlog)

## Related issues
- TRK-5 Nearby (backlog)

## Linked documents
- Design note (document 1)

## Project rules
Keep it small.

{}",
        how_to_work("http://aurora.test:7878/mcp")
    )
}

// ---- events, replay and the invariants ----

const SENTINEL: &str = "SENTINEL-body-text-7f3a";

async fn save_text(fx: &Fx, at: i64, text: String) {
    let scribe = fx.scribe;
    fx.store
        .write(move |tx| save_template(tx, at, &owner(), scribe, Some("TRK"), None, &text))
        .await
        .unwrap();
}

#[tokio::test]
async fn template_events_carry_no_text_and_name_the_owner() {
    let fx = fixture().await;
    let scribe = fx.scribe;
    let first = fx
        .store
        .write(move |tx| {
            save_template(
                tx,
                100,
                &owner(),
                scribe,
                Some("TRK"),
                None,
                &format!("{SENTINEL} {{{{issue.key}}}}"),
            )
        })
        .await
        .unwrap();
    save_text(&fx, 101, format!("{SENTINEL} changed")).await;
    // Saving the same text again writes nothing.
    save_text(&fx, 102, format!("{SENTINEL} changed")).await;
    let id = first.template.id;
    fx.store
        .write(move |tx| prompt::delete_template(tx, 103, &owner(), scribe, id))
        .await
        .unwrap();

    let found = fx.events("prompt_template.").await;
    let kinds: Vec<&str> = found.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "prompt_template.saved",
            "prompt_template.saved",
            "prompt_template.deleted"
        ]
    );
    for event in &found {
        assert_eq!(event.actor, Some(crate::tests_support::TEST_USER_ID));
        assert_eq!(event.entity_kind, "agent");
        assert_eq!(event.entity_id, scribe);
        assert_eq!(event.project_id, None);
        assert_eq!(event.payload["agent_id"], scribe);
        assert_eq!(event.payload["project"], "TRK");
    }
    assert_eq!(found[0].payload["created"], true);
    assert_eq!(found[1].payload["created"], false);
    assert_eq!(found[0].payload["body"]["changed"], true);
    assert_eq!(
        found[0].payload["body"]["chars"],
        format!("{SENTINEL} {{{{issue.key}}}}").chars().count()
    );
    assert_eq!(found[2].payload.get("body"), None);

    // The sentinel is nowhere in the event log.
    let leaked: i64 = fx
        .store
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM event WHERE payload_json LIKE ?1 OR kind LIKE ?1",
                [format!("%{SENTINEL}%")],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(leaked, 0);
    fx.store.close().await;
}

async fn update_rules(
    fx: &Fx,
    at: i64,
    name: Option<&str>,
    rules: Option<Option<String>>,
) -> Result<crate::projects::Project, StoreError> {
    let name = name.map(String::from);
    fx.store
        .write(move |tx| {
            projects::update_project(
                tx,
                at,
                &test_actor(tx),
                "TRK",
                projects::ProjectPatch {
                    name,
                    rules,
                    ..projects::ProjectPatch::default()
                },
            )
        })
        .await
}

#[tokio::test]
async fn project_rules_events_carry_no_text_either() {
    let fx = fixture().await;
    let text = format!("{SENTINEL} rules");
    let project = update_rules(&fx, 110, None, Some(Some(text.clone())))
        .await
        .unwrap();
    assert_eq!(project.rules, text);
    // The same rules again, and clearing what is already clear, change nothing.
    update_rules(&fx, 111, None, Some(Some(text.clone())))
        .await
        .unwrap();
    let cleared = update_rules(&fx, 112, None, Some(None)).await.unwrap();
    assert_eq!(cleared.rules, "");
    update_rules(&fx, 113, None, Some(None)).await.unwrap();
    let blank = update_rules(&fx, 114, None, Some(Some("  \n".into())))
        .await
        .unwrap();
    assert_eq!(blank.rules, "");
    // Name and rules together are one event.
    update_rules(&fx, 115, Some("Tracker 2"), Some(Some("r".into())))
        .await
        .unwrap();

    let found = fx.events("project.updated").await;
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(
        found[0].payload,
        json!({"rules": {"changed": true, "chars": text.chars().count()}})
    );
    assert_eq!(
        found[1].payload,
        json!({"rules": {"changed": true, "chars": 0}})
    );
    assert_eq!(found[2].payload["name"], "Tracker 2");
    assert_eq!(found[2].payload["rules"]["chars"], 1);
    let leaked: i64 = fx
        .store
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM event WHERE payload_json LIKE ?1",
                [format!("%{SENTINEL}%")],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(leaked, 0);

    let err = update_rules(&fx, 116, None, None).await.unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    let too_long = "r".repeat(crate::projects::MAX_RULES_CHARS + 1);
    let err = update_rules(&fx, 117, None, Some(Some(too_long)))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    let at_limit = "r".repeat(crate::projects::MAX_RULES_CHARS);
    update_rules(&fx, 118, None, Some(Some(at_limit)))
        .await
        .unwrap();
    fx.store.close().await;
}

#[tokio::test]
async fn a_failed_write_leaves_neither_a_template_nor_an_event() {
    let fx = fixture().await;
    let scribe = fx.scribe;
    let err = fx
        .store
        .write(move |tx| {
            save_template(
                tx,
                120,
                &owner(),
                scribe,
                Some("TRK"),
                None,
                "will be rolled back",
            )?;
            projects::update_project(
                tx,
                121,
                &owner(),
                "TRK",
                projects::ProjectPatch {
                    rules: Some(Some("rolled back too".into())),
                    ..projects::ProjectPatch::default()
                },
            )?;
            Err::<(), _>(crate::projects::conflict("the rest of the change failed"))
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Conflict(_)));
    assert!(fx.events("prompt_template.").await.is_empty());
    assert!(fx.events("project.updated").await.is_empty());
    let (templates, rules): (i64, i64) = fx
        .store
        .read(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM prompt_template", [], |r| r.get(0))?,
                c.query_row(
                    "SELECT COUNT(*) FROM project WHERE rules IS NOT NULL",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((templates, rules), (0, 0));
    fx.store.close().await;
}

#[tokio::test]
async fn the_replay_agrees_with_the_tables_after_every_kind_of_change() {
    let fx = fixture().await;
    let (scribe, plain) = (fx.scribe, fx.plain);
    let check = |label: &'static str| {
        let store = fx.store.clone();
        async move {
            let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
            for name in ["events.state_agreement", "prompt_templates.valid"] {
                let result = report.get(name).unwrap();
                assert!(result.ok, "{label}: {name}: {}", result.details);
            }
        }
    };
    check("fresh").await;
    let a = fx.save(scribe, "TRK", None, "a {{issue.key}}").await;
    check("saved").await;
    fx.save(scribe, "TRK", Some(fx.task), "typed").await;
    fx.save(plain, "OTH", Some(fx.item), "other agent").await;
    fx.save(scribe, "TRK", None, "a, longer text").await;
    check("replaced").await;
    let id = a.template.id;
    fx.store
        .write(move |tx| prompt::delete_template(tx, 130, &owner(), scribe, id))
        .await
        .unwrap();
    check("deleted").await;
    fx.store
        .write(|tx| {
            projects::update_project(
                tx,
                131,
                &owner(),
                "OTH",
                projects::ProjectPatch {
                    rules: Some(Some("Other rules".into())),
                    ..projects::ProjectPatch::default()
                },
            )
        })
        .await
        .unwrap();
    check("rules").await;
    // Deleting an agent takes its overrides along, in the tables and the replay.
    fx.store
        .write(move |tx| agents::delete_agent(tx, 132, &owner(), plain))
        .await
        .unwrap();
    check("agent deleted").await;
    let left: i64 = fx
        .store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM prompt_template", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(left, 1);
    fx.store.close().await;
}

#[test]
fn public_urls_are_checked_and_normalized() {
    let ok = [
        ("http://localhost:7878", "http://localhost:7878"),
        ("https://aurora.example.com/", "https://aurora.example.com"),
        ("  http://10.0.0.5:8080//  ", "http://10.0.0.5:8080"),
        (
            "https://example.com/base/path",
            "https://example.com/base/path",
        ),
        ("http://[::1]:7878", "http://[::1]:7878"),
    ];
    for (given, expected) in ok {
        assert_eq!(
            crate::normalize_public_url(given).as_deref(),
            Ok(expected),
            "{given}"
        );
    }
    let long = format!("http://{}", "a".repeat(250));
    let bad = [
        "",
        "localhost:7878",
        "ftp://example.com",
        "http://",
        "http:///path",
        "http://user:pw@example.com",
        "http://example.com?x=1",
        "http://example.com/#frag",
        "http://exa mple.com",
        "javascript:alert(1)",
        long.as_str(),
    ];
    for given in bad {
        assert!(crate::normalize_public_url(given).is_err(), "{given:?}");
    }
}
