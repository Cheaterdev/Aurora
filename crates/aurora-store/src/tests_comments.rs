//! Comments: threading, permissions, soft delete, mentions and events.

use serde_json::{Value, json};

use crate::auth::{Principal, Role, Scope};
use crate::comments::{
    self, Comment, MAX_BODY_CHARS, NewComment, SNIPPET_CHARS, mention_ranges, scan_mentions,
};
use crate::events::{self, Event, EventFilter};
use crate::projects::Rejection;
use crate::tests_support::issues::{NewIssue, create_issue};
use crate::tests_support::projects::{create_issue_type, create_project};
use crate::{Store, StoreError};

const OWNER: Principal = principal(1, Role::Owner);
const ANN: Principal = principal(2, Role::Member);
const BOB: Principal = principal(3, Role::Member);
const ROBOT: Principal = principal(4, Role::Agent);

const fn principal(user_id: i64, role: Role) -> Principal {
    Principal {
        user_id,
        role,
        scope: Scope::Write,
    }
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

fn is_invalid(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::Invalid(_))
}

/// Users: 1 Owner, 2 ann, 3 bob, 4 robot (agent), 5 ghost (disabled), 6 a,
/// 7 a.b, 8 x_y-z. Projects TRK (issues 1 and 2) and OTH (issue 1).
async fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO user (id, name, kind, role, disabled) VALUES
                    (1, 'Owner', 'human', 'owner', 0),
                    (2, 'ann', 'human', 'member', 0),
                    (3, 'bob', 'human', 'member', 0),
                    (4, 'robot', 'agent', 'agent', 0),
                    (5, 'ghost', 'human', 'member', 1),
                    (6, 'a', 'human', 'member', 0),
                    (7, 'a.b', 'human', 'member', 0),
                    (8, 'x_y-z', 'human', 'member', 0);",
            )?;
            for key in ["TRK", "OTH"] {
                create_project(tx, 1, key, key, None)?;
                create_issue_type(tx, 1, key, "Task")?;
                create_issue(
                    tx,
                    2,
                    key,
                    NewIssue {
                        title: "one".into(),
                        ..NewIssue::default()
                    },
                )?;
            }
            create_issue(
                tx,
                2,
                "TRK",
                NewIssue {
                    title: "two".into(),
                    ..NewIssue::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (dir, store)
}

async fn add(
    s: &Store,
    who: Principal,
    issue: i64,
    body: &str,
    parent: Option<i64>,
) -> Result<Comment, StoreError> {
    let body = body.to_string();
    s.write(move |tx| {
        comments::add_comment(tx, 100, &who, "TRK", issue, NewComment { body, parent })
    })
    .await
}

async fn edit(s: &Store, who: Principal, id: i64, body: &str) -> Result<Comment, StoreError> {
    let body = body.to_string();
    s.write(move |tx| comments::edit_comment(tx, 200, &who, "TRK", id, &body))
        .await
}

async fn delete(s: &Store, who: Principal, id: i64) -> Result<(), StoreError> {
    s.write(move |tx| comments::delete_comment(tx, 300, &who, "TRK", id))
        .await
}

async fn list(s: &Store, issue: i64) -> Vec<Comment> {
    s.read(move |c| comments::list_comments(c, "TRK", issue))
        .await
        .unwrap()
}

async fn log(s: &Store, kind: &str) -> Vec<Event> {
    let kind = kind.to_string();
    s.read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

fn ids(list: &[Comment]) -> Vec<i64> {
    list.iter().map(|c| c.id).collect()
}

fn names(comment: &Comment) -> Vec<&str> {
    comment.mentions.iter().map(|m| m.name.as_str()).collect()
}

// ---- add, edit, delete ----

#[tokio::test]
async fn comments_can_be_added_edited_and_deleted() {
    let (_dir, s) = setup().await;
    let c = add(&s, ANN, 1, "  first thought \n", None).await.unwrap();
    assert_eq!(c.body, "first thought");
    assert_eq!(c.issue.identifier, "TRK-1");
    assert_eq!((c.author.id, c.author.name.as_str()), (2, "ann"));
    assert_eq!(c.author.kind, "human");
    assert_eq!(
        (c.created, c.edited, c.deleted, c.parent),
        (100, None, None, None)
    );
    assert!(c.replies.is_empty() && c.mentions.is_empty());

    let e = edit(&s, ANN, c.id, "second thought").await.unwrap();
    assert_eq!(
        (e.body.as_str(), e.created, e.edited),
        ("second thought", 100, Some(200))
    );
    let got = s
        .read(move |conn| comments::get_comment(conn, "TRK", c.id))
        .await
        .unwrap();
    assert_eq!(got, e);

    delete(&s, ANN, c.id).await.unwrap();
    assert!(list(&s, 1).await.is_empty(), "no replies: hidden");
    // A deleted comment can be neither edited nor deleted again, and reads blank.
    assert!(matches!(
        rejection(edit(&s, ANN, c.id, "again").await.unwrap_err()),
        Rejection::NotFound(_)
    ));
    assert!(matches!(
        rejection(delete(&s, ANN, c.id).await.unwrap_err()),
        Rejection::NotFound(_)
    ));
    let got = s
        .read(move |conn| comments::get_comment(conn, "TRK", c.id))
        .await
        .unwrap();
    assert_eq!((got.body.as_str(), got.deleted), ("", Some(300)));
    s.close().await;
}

#[tokio::test]
async fn body_length_is_checked_after_trimming() {
    let (_dir, s) = setup().await;
    for bad in ["", "   \n\t ", &"x".repeat(MAX_BODY_CHARS + 1)] {
        assert!(is_invalid(add(&s, ANN, 1, bad, None).await.unwrap_err()));
    }
    add(
        &s,
        ANN,
        1,
        &format!("  {}  ", "x".repeat(MAX_BODY_CHARS)),
        None,
    )
    .await
    .unwrap();
    // Characters, not bytes.
    let wide = "\u{e9}".repeat(MAX_BODY_CHARS);
    let c = add(&s, ANN, 1, &wide, None).await.unwrap();
    assert!(is_invalid(edit(&s, ANN, c.id, "  ").await.unwrap_err()));
    assert_eq!(list(&s, 1).await.len(), 2);
    s.close().await;
}

#[tokio::test]
async fn unknown_issue_and_project_are_not_found() {
    let (_dir, s) = setup().await;
    assert!(matches!(
        rejection(add(&s, ANN, 99, "x", None).await.unwrap_err()),
        Rejection::NotFound(_)
    ));
    let err = s
        .write(|tx| {
            comments::add_comment(
                tx,
                1,
                &ANN,
                "NOPE",
                1,
                NewComment {
                    body: "x".into(),
                    parent: None,
                },
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::NotFound(_)));
    let err = s
        .read(|c| comments::list_comments(c, "TRK", 99))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::NotFound(_)));
    s.close().await;
}

// ---- threading ----

#[tokio::test]
async fn replies_nest_one_level_and_only_under_a_live_comment_of_the_same_issue() {
    let (_dir, s) = setup().await;
    let top = add(&s, ANN, 1, "top", None).await.unwrap();
    let reply = add(&s, BOB, 1, "reply", Some(top.id)).await.unwrap();
    assert_eq!(reply.parent, Some(top.id));

    // A reply to a reply, a parent on another issue or project, a missing
    // parent and a deleted parent are all refused.
    assert!(is_invalid(
        add(&s, BOB, 1, "nested", Some(reply.id)).await.unwrap_err()
    ));
    assert!(is_invalid(
        add(&s, BOB, 2, "elsewhere", Some(top.id))
            .await
            .unwrap_err()
    ));
    assert!(is_invalid(
        add(&s, BOB, 1, "missing", Some(9999)).await.unwrap_err()
    ));
    let other = s
        .write(|tx| {
            comments::add_comment(
                tx,
                1,
                &ANN,
                "OTH",
                1,
                NewComment {
                    body: "other project".into(),
                    parent: None,
                },
            )
        })
        .await
        .unwrap();
    assert!(is_invalid(
        add(&s, BOB, 1, "cross", Some(other.id)).await.unwrap_err()
    ));
    let doomed = add(&s, ANN, 1, "doomed", None).await.unwrap();
    delete(&s, ANN, doomed.id).await.unwrap();
    assert!(is_invalid(
        add(&s, BOB, 1, "late", Some(doomed.id)).await.unwrap_err()
    ));

    let all = list(&s, 1).await;
    assert_eq!(ids(&all), [top.id]);
    assert_eq!(ids(&all[0].replies), [reply.id]);
    s.close().await;
}

#[tokio::test]
async fn threads_list_in_creation_order() {
    let (_dir, s) = setup().await;
    let a = add(&s, ANN, 1, "a", None).await.unwrap();
    let b = add(&s, ANN, 1, "b", None).await.unwrap();
    let b1 = add(&s, BOB, 1, "b1", Some(b.id)).await.unwrap();
    let a1 = add(&s, BOB, 1, "a1", Some(a.id)).await.unwrap();
    let b2 = add(&s, ANN, 1, "b2", Some(b.id)).await.unwrap();
    let c = add(&s, ANN, 1, "c", None).await.unwrap();
    add(&s, ANN, 2, "elsewhere", None).await.unwrap();
    let all = list(&s, 1).await;
    assert_eq!(ids(&all), [a.id, b.id, c.id]);
    assert_eq!(ids(&all[0].replies), [a1.id]);
    assert_eq!(ids(&all[1].replies), [b1.id, b2.id]);
    assert!(all[2].replies.is_empty());
    assert!(all[1].replies.iter().all(|r| r.replies.is_empty()));
    s.close().await;
}

#[tokio::test]
async fn a_deleted_comment_is_blank_but_keeps_its_replies() {
    let (_dir, s) = setup().await;
    let top = add(&s, ANN, 1, "secret @bob", None).await.unwrap();
    let kept = add(&s, BOB, 1, "stays", Some(top.id)).await.unwrap();
    let gone = add(&s, BOB, 1, "removed", Some(top.id)).await.unwrap();
    delete(&s, BOB, gone.id).await.unwrap();
    delete(&s, ANN, top.id).await.unwrap();
    let all = list(&s, 1).await;
    assert_eq!(all.len(), 1);
    let placeholder = &all[0];
    assert_eq!(placeholder.id, top.id);
    assert_eq!(placeholder.body, "");
    assert_eq!(placeholder.deleted, Some(300));
    assert!(placeholder.mentions.is_empty());
    assert_eq!(placeholder.author.name, "ann");
    assert_eq!(
        ids(&placeholder.replies),
        [kept.id],
        "a deleted reply is left out"
    );
    assert_eq!(placeholder.replies[0].body, "stays");

    // With its last reply gone the placeholder goes too.
    delete(&s, BOB, kept.id).await.unwrap();
    assert!(list(&s, 1).await.is_empty());
    s.close().await;
}

// ---- permissions ----

#[tokio::test]
async fn only_the_author_and_the_owner_change_a_comment() {
    let (_dir, s) = setup().await;
    let mine = add(&s, ANN, 1, "ann's", None).await.unwrap();
    let robots = add(&s, ROBOT, 1, "robot's", None).await.unwrap();
    let forbidden = |err: StoreError| matches!(rejection(err), Rejection::Forbidden(_));

    assert!(forbidden(edit(&s, BOB, mine.id, "x").await.unwrap_err()));
    assert!(forbidden(edit(&s, ROBOT, mine.id, "x").await.unwrap_err()));
    assert!(forbidden(delete(&s, BOB, mine.id).await.unwrap_err()));
    assert!(forbidden(delete(&s, ROBOT, mine.id).await.unwrap_err()));
    assert!(forbidden(edit(&s, ANN, robots.id, "x").await.unwrap_err()));
    assert_eq!(list(&s, 1).await[0].body, "ann's");

    edit(&s, ANN, mine.id, "ann's, edited").await.unwrap();
    edit(&s, ROBOT, robots.id, "robot's, edited").await.unwrap();
    // The owner may change anybody's; the editor is the actor, the author stays.
    let by_owner = edit(&s, OWNER, mine.id, "owner was here").await.unwrap();
    assert_eq!(by_owner.author.id, 2);
    delete(&s, OWNER, robots.id).await.unwrap();
    delete(&s, ANN, mine.id).await.unwrap();

    let edited = log(&s, "comment.edited").await;
    assert_eq!(
        edited.iter().map(|e| e.actor).collect::<Vec<_>>(),
        [Some(2), Some(4), Some(1)]
    );

    // A comment of another project is not found, whoever asks.
    let top = add(&s, ANN, 1, "x", None).await.unwrap();
    let err = s
        .write(move |tx| comments::edit_comment(tx, 1, &OWNER, "OTH", top.id, "y"))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::NotFound(_)));
    let err = s
        .write(move |tx| comments::delete_comment(tx, 1, &OWNER, "OTH", top.id))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::NotFound(_)));
    s.close().await;
}

// ---- mention parsing ----

fn parse(body: &str, known: &[&str]) -> Vec<String> {
    scan_mentions(body, |candidate| {
        known
            .iter()
            .find(|n| n.eq_ignore_ascii_case(candidate))
            .map(|n| (*n).to_string())
    })
    .into_iter()
    .map(|(_, name)| name)
    .collect()
}

#[test]
fn mention_parsing_table() {
    let known = ["alice", "bob", "a", "a.b", "x_y-z"];
    let cases: &[(&str, &[&str])] = &[
        ("@alice", &["alice"]),
        ("@alice hello", &["alice"]),
        ("hello @alice", &["alice"]),
        ("(@alice)", &["alice"]),
        ("see,@alice.", &["alice"]),
        ("\"@alice\"", &["alice"]),
        ("line one\n@alice", &["alice"]),
        ("@alice and @bob", &["alice", "bob"]),
        ("@ALICE", &["alice"]),
        ("@AlIcE", &["alice"]),
        ("@alice @alice @ALICE", &["alice", "alice", "alice"]),
        ("@a.b", &["a.b"]),
        ("@a.b.", &["a.b"]),
        ("@a.b, @a!", &["a.b", "a"]),
        ("@a.c", &["a"]),
        ("@a-b", &["a"]),
        ("@x_y-z", &["x_y-z"]),
        ("@x_y-z!", &["x_y-z"]),
        ("@alice's", &["alice"]),
        ("@alicex", &[]),
        ("@alice_x", &[]),
        ("@alice-x", &["alice"]),
        ("@nobody", &[]),
        ("@", &[]),
        ("@@alice", &[]),
        ("a@b.com", &[]),
        ("mail alice@alice.com", &[]),
        ("x_@alice", &[]),
        ("a.@alice", &[]),
        ("+@alice", &[]),
        ("\u{e9}@alice", &[]),
        ("caf\u{e9} @alice", &["alice"]),
        ("@\u{e9}alice", &[]),
        ("", &[]),
        ("no mentions here", &[]),
    ];
    for (body, expected) in cases {
        assert_eq!(parse(body, &known), *expected, "{body:?}");
    }
}

#[test]
fn mention_ranges_cover_the_at_sign_and_the_name() {
    let body = "hi @Alice, and \u{e9}@bob and @bob!";
    let ranges = mention_ranges(body, &["alice", "bob"]);
    let texts: Vec<&str> = ranges.iter().map(|r| &body[r.clone()]).collect();
    assert_eq!(texts, ["@Alice", "@bob"]);
    assert!(mention_ranges("@alice", &[]).is_empty());
    // Very long runs are bounded and still fine.
    let long = format!("@{}", "a".repeat(5000));
    assert!(mention_ranges(&long, &["a"]).is_empty());
}

#[tokio::test]
async fn only_existing_enabled_users_are_mentioned_once_each() {
    let (_dir, s) = setup().await;
    let c = add(
        &s,
        ANN,
        1,
        "@robot @ghost @nobody @Bob @robot a@bob.com @a.b @a.",
        None,
    )
    .await
    .unwrap();
    assert_eq!(names(&c), ["robot", "bob", "a.b", "a"]);
    assert_eq!(
        c.mentions.iter().map(|m| m.user_id).collect::<Vec<_>>(),
        [4, 3, 7, 6]
    );
    let again = list(&s, 1).await;
    assert_eq!(names(&again[0]), ["robot", "bob", "a.b", "a"]);
    s.close().await;
}

// ---- events ----

#[tokio::test]
async fn created_comments_and_mentions_emit_events_with_the_actor() {
    let (_dir, s) = setup().await;
    let top = add(&s, ANN, 1, "hello @robot and @bob", None)
        .await
        .unwrap();
    let reply = add(&s, BOB, 1, "thanks @ann", Some(top.id)).await.unwrap();
    let issue_id: i64 = s
        .read(|c| c.query_row("SELECT id FROM issue WHERE number = 1 AND project_id = (SELECT id FROM project WHERE key = 'TRK')", [], |r| r.get(0)).map_err(Into::into))
        .await
        .unwrap();

    let created = log(&s, "comment.created").await;
    assert_eq!(created.len(), 2);
    assert_eq!(created[0].actor, Some(2));
    assert_eq!(created[0].project.as_deref(), Some("TRK"));
    assert_eq!(
        (created[0].entity_kind.as_str(), created[0].entity_id),
        ("issue", issue_id)
    );
    assert_eq!(
        created[0].payload,
        json!({
            "comment_id": top.id,
            "parent": null,
            "author": {"id": 2, "name": "ann", "kind": "human"},
            "mentions": ["robot", "bob"],
        })
    );
    assert_eq!(created[1].actor, Some(3));
    assert_eq!(created[1].payload["parent"], json!(top.id));

    // The payload the runner reads: complete, one event per mentioned user.
    let mentions = log(&s, "mention.created").await;
    assert_eq!(mentions.len(), 3);
    assert_eq!(
        mentions[0].payload,
        json!({
            "comment_id": top.id,
            "issue": {"number": 1, "identifier": "TRK-1"},
            "user_id": 4,
            "user_name": "robot",
            "user_kind": "agent",
            "author_id": 2,
            "snippet": "hello @robot and @bob",
        })
    );
    assert_eq!(mentions[1].payload["user_kind"], "human");
    assert_eq!(mentions[1].payload["user_name"], "bob");
    assert_eq!(mentions[2].payload["comment_id"], json!(reply.id));
    assert!(
        mentions
            .iter()
            .all(|e| e.entity_kind == "issue" && e.entity_id == issue_id)
    );
    assert_eq!(mentions[2].actor, Some(3));
    s.close().await;
}

#[tokio::test]
async fn a_user_mentioning_themselves_still_emits() {
    let (_dir, s) = setup().await;
    add(&s, ROBOT, 1, "note to @robot", None).await.unwrap();
    let mentions = log(&s, "mention.created").await;
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].payload["user_name"], "robot");
    assert_eq!(mentions[0].actor, Some(4));
    s.close().await;
}

#[tokio::test]
async fn only_the_body_snippet_carries_comment_text() {
    let (_dir, s) = setup().await;
    let tail = "TAIL-MARKER";
    let body = format!("@robot {}{tail}", "y".repeat(400));
    let c = add(&s, ANN, 1, &body, None).await.unwrap();
    edit(&s, ANN, c.id, &format!("{body} @bob")).await.unwrap();
    delete(&s, ANN, c.id).await.unwrap();

    let all = s
        .read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
        .await
        .unwrap();
    let mut saw_snippet = 0;
    for event in all
        .iter()
        .filter(|e| e.kind.starts_with("comment.") || e.kind.starts_with("mention."))
    {
        let text = event.payload.to_string();
        assert!(
            !text.contains(tail),
            "{} leaks the body: {text}",
            event.kind
        );
        assert!(
            !text.contains(&"y".repeat(SNIPPET_CHARS + 1)),
            "{}",
            event.kind
        );
        if let Some(snippet) = event.payload.get("snippet") {
            assert_eq!(snippet.as_str().unwrap().chars().count(), SNIPPET_CHARS);
            assert!(snippet.as_str().unwrap().starts_with("@robot yyy"));
            saw_snippet += 1;
        }
    }
    assert_eq!(
        saw_snippet, 2,
        "one for the first @robot, one for the added @bob"
    );
    s.close().await;
}

#[tokio::test]
async fn editing_emits_mention_events_only_for_new_mentions() {
    let (_dir, s) = setup().await;
    let c = add(&s, ANN, 1, "@robot first", None).await.unwrap();
    assert_eq!(log(&s, "mention.created").await.len(), 1);

    // Same mention kept, one added.
    let e = edit(&s, ANN, c.id, "@robot first, plus @bob")
        .await
        .unwrap();
    assert_eq!(names(&e), ["robot", "bob"]);
    let mentions = log(&s, "mention.created").await;
    assert_eq!(mentions.len(), 2);
    assert_eq!(mentions[1].payload["user_name"], "bob");
    assert_eq!(mentions[1].payload["snippet"], "@robot first, plus @bob");
    assert_eq!(mentions[1].actor, Some(2));

    // One dropped, none added: no event, and the dropped one is forgotten.
    let e = edit(&s, ANN, c.id, "only @bob now").await.unwrap();
    assert_eq!(names(&e), ["bob"]);
    assert_eq!(log(&s, "mention.created").await.len(), 2);

    // Mentioned again later: new relative to the current body, so it emits.
    edit(&s, ANN, c.id, "@robot is back, @bob stays")
        .await
        .unwrap();
    let mentions = log(&s, "mention.created").await;
    assert_eq!(mentions.len(), 3);
    assert_eq!(mentions[2].payload["user_name"], "robot");

    let edited = log(&s, "comment.edited").await;
    assert_eq!(edited.len(), 3);
    assert_eq!(
        edited[1].payload,
        json!({"comment_id": c.id, "mentions": ["bob"]})
    );

    // The owner editing someone else's comment: attributed to the owner, and
    // the mention still names the comment's author.
    edit(&s, OWNER, c.id, "@robot is back, @bob stays, @ann too")
        .await
        .unwrap();
    let last = log(&s, "mention.created").await.pop().unwrap();
    assert_eq!(
        (last.actor, &last.payload["author_id"]),
        (Some(1), &json!(2))
    );

    // An edit that changes nothing writes nothing.
    let before = log(&s, "comment.edited").await.len();
    let same = edit(&s, ANN, c.id, "  @robot is back, @bob stays, @ann too ")
        .await
        .unwrap();
    assert_eq!(same.edited, Some(200));
    assert_eq!(log(&s, "comment.edited").await.len(), before);
    s.close().await;
}

#[tokio::test]
async fn delete_emits_one_event_and_a_failed_write_leaves_nothing() {
    let (_dir, s) = setup().await;
    let c = add(&s, ANN, 1, "x @robot", None).await.unwrap();
    delete(&s, ANN, c.id).await.unwrap();
    let deleted = log(&s, "comment.deleted").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].payload, json!({"comment_id": c.id}));
    assert_eq!(deleted[0].actor, Some(2));

    let events_before =
        log(&s, "comment.created").await.len() + log(&s, "mention.created").await.len();
    let err = s
        .write(|tx| {
            comments::add_comment(
                tx,
                1,
                &ANN,
                "TRK",
                1,
                NewComment {
                    body: "@robot @bob rolled back".into(),
                    parent: None,
                },
            )?;
            Err::<(), _>(StoreError::Closed)
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Closed));
    let events_after =
        log(&s, "comment.created").await.len() + log(&s, "mention.created").await.len();
    assert_eq!(events_after, events_before);
    let count: i64 = s
        .read(|c| {
            c.query_row("SELECT COUNT(*) FROM comment", [], |r| r.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(count, 1, "only the first comment exists");
    s.close().await;
}

#[test]
fn comment_json_shape() {
    let value: Value = serde_json::to_value(Comment {
        id: 1,
        issue: crate::links::IssueRef {
            number: 3,
            identifier: "TRK-3".into(),
        },
        parent: None,
        author: comments::CommentAuthor {
            id: 2,
            name: "ann".into(),
            kind: "human".into(),
        },
        body: "b".into(),
        created: 5,
        edited: None,
        deleted: None,
        mentions: vec![comments::MentionedUser {
            user_id: 4,
            name: "robot".into(),
        }],
        replies: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        value,
        json!({
            "id": 1,
            "issue": {"number": 3, "identifier": "TRK-3"},
            "parent": null,
            "author": {"id": 2, "name": "ann", "kind": "human"},
            "body": "b",
            "created": 5,
            "edited": null,
            "deleted": null,
            "mentions": [{"user_id": 4, "name": "robot"}],
            "replies": [],
        })
    );
}
