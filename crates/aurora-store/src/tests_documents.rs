//! Documents: revisions, concurrency, parents, ordering, archiving, links, events.

use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::json;

use crate::auth::{Principal, Role, Scope};
use crate::documents::{
    self as store, DocumentDetail, DocumentLink, DocumentMove, DocumentPatch, DocumentSummary,
    DocumentTarget, MAX_BODY_CHARS, MAX_DEPTH, MAX_TITLE_CHARS, NewDocument, in_tree_order,
};
use crate::events::{self, Event, EventFilter};
use crate::projects::Rejection;
use crate::tests_support::initiatives::{self, NewInitiative};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::projects::{create_issue_type, create_project};
use crate::{Store, StoreError};

const ROBOT: Principal = Principal {
    user_id: 9,
    role: Role::Agent,
    scope: Scope::Write,
};

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

fn is_invalid(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::Invalid(_))
}

fn is_not_found(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::NotFound(_))
}

fn is_conflict(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::Conflict(_))
}

struct S {
    _dir: tempfile::TempDir,
    store: Store,
    clock: AtomicI64,
}

/// Projects TRK (issues 1 and 2, initiative 1) and OTH (issue 1, initiative 2).
async fn setup() -> S {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind) VALUES (9, 'robot', 'agent')",
                [],
            )?;
            for key in ["TRK", "OTH"] {
                create_project(tx, 1, key, key, None)?;
                create_issue_type(tx, 1, key, "Task")?;
                let titles: &[&str] = if key == "TRK" {
                    &["one", "two"]
                } else {
                    &["theirs"]
                };
                for title in titles {
                    issues::create_issue(
                        tx,
                        1,
                        key,
                        NewIssue {
                            title: (*title).into(),
                            ..NewIssue::default()
                        },
                    )?;
                }
                initiatives::create_initiative(
                    tx,
                    1,
                    key,
                    NewInitiative {
                        name: format!("{key} initiative"),
                        ..NewInitiative::default()
                    },
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    S {
        _dir: dir,
        store,
        clock: AtomicI64::new(1000),
    }
}

fn new(title: &str) -> NewDocument {
    NewDocument {
        title: title.into(),
        ..NewDocument::default()
    }
}

fn under(title: &str, parent: i64) -> NewDocument {
    NewDocument {
        title: title.into(),
        parent: Some(parent),
        ..NewDocument::default()
    }
}

fn body(text: &str) -> DocumentPatch {
    DocumentPatch {
        body: Some(text.into()),
        ..DocumentPatch::default()
    }
}

impl S {
    fn now(&self) -> i64 {
        self.clock.fetch_add(10, Ordering::SeqCst)
    }

    async fn create_in(
        &self,
        key: &'static str,
        new: NewDocument,
    ) -> Result<DocumentDetail, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::create_document(tx, now, &ROBOT, key, new))
            .await
    }

    async fn create(&self, new: NewDocument) -> Result<DocumentDetail, StoreError> {
        self.create_in("TRK", new).await
    }

    /// A TRK document; returns its id.
    async fn doc(&self, title: &str) -> i64 {
        self.create(new(title)).await.unwrap().document.id
    }

    async fn child(&self, title: &str, parent: i64) -> i64 {
        self.create(under(title, parent)).await.unwrap().document.id
    }

    async fn update(&self, id: i64, patch: DocumentPatch) -> Result<DocumentDetail, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::update_document(tx, now, &ROBOT, "TRK", id, patch))
            .await
    }

    async fn mv(&self, id: i64, target: DocumentMove) -> Result<DocumentDetail, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::move_document(tx, now, &ROBOT, "TRK", id, target))
            .await
    }

    async fn archive(&self, id: i64) -> Result<DocumentDetail, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::archive_document(tx, now, &ROBOT, "TRK", id))
            .await
    }

    async fn unarchive(&self, id: i64) -> Result<DocumentDetail, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::unarchive_document(tx, now, &ROBOT, "TRK", id))
            .await
    }

    async fn restore(
        &self,
        id: i64,
        number: i64,
        base: Option<i64>,
    ) -> Result<DocumentDetail, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::restore_revision(tx, now, &ROBOT, "TRK", id, number, base))
            .await
    }

    async fn link(&self, id: i64, target: DocumentTarget) -> Result<DocumentLink, StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::add_link(tx, now, &ROBOT, "TRK", id, target))
            .await
    }

    async fn unlink(&self, id: i64, link_id: i64) -> Result<(), StoreError> {
        let now = self.now();
        self.store
            .write(move |tx| store::remove_link(tx, now, &ROBOT, "TRK", id, link_id))
            .await
    }

    async fn get(&self, id: i64) -> DocumentDetail {
        self.store
            .read(move |c| store::get_document(c, "TRK", id))
            .await
            .unwrap()
    }

    async fn list(&self, archived: bool) -> Vec<DocumentSummary> {
        self.store
            .read(move |c| store::list_documents(c, "TRK", archived))
            .await
            .unwrap()
    }

    async fn titles(&self) -> Vec<(usize, String)> {
        in_tree_order(self.list(false).await)
            .into_iter()
            .map(|(depth, d)| (depth, d.title))
            .collect()
    }

    async fn log(&self, prefix: &str) -> Vec<Event> {
        let prefix = prefix.to_string();
        self.store
            .read(move |c| {
                events::read_events(
                    c,
                    0,
                    Some(500),
                    &EventFilter {
                        kind_prefix: Some(prefix),
                        ..EventFilter::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn event_count(&self) -> i64 {
        self.store
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    async fn revisions(&self, id: i64) -> Vec<store::RevisionSummary> {
        self.store
            .read(move |c| store::list_revisions(c, "TRK", id))
            .await
            .unwrap()
    }

    async fn revision(&self, id: i64, number: i64) -> Result<store::Revision, StoreError> {
        self.store
            .read(move |c| store::get_revision(c, "TRK", id, number))
            .await
    }
}

// ---- create and read ----

#[tokio::test]
async fn create_writes_revision_one_and_the_defaults() {
    let s = setup().await;
    let made = s
        .create(NewDocument {
            title: "  Spec  ".into(),
            body: Some("# Heading\n\ntext".into()),
            parent: None,
        })
        .await
        .unwrap();
    let d = &made.document;
    assert_eq!(d.title, "Spec", "the title is trimmed");
    assert_eq!(d.body, "# Heading\n\ntext", "the body is kept as written");
    assert_eq!((d.revision, d.parent, d.archived), (1, None, false));
    assert_eq!((d.created, d.updated), (1000, 1000));
    assert_eq!((d.created_by, d.updated_by), (Some(9), Some(9)));
    assert_eq!(d.position, 1024.0);
    assert!(made.children.is_empty() && made.ancestors.is_empty());
    assert!(made.links.issues.is_empty() && made.links.initiatives.is_empty());

    let plain = s.create(new("Plain")).await.unwrap();
    assert_eq!(plain.document.body, "");
    assert_eq!(plain.document.position, 2048.0, "new documents go last");

    let history = s.revisions(d.id).await;
    assert_eq!(history.len(), 1);
    let first = &history[0];
    assert_eq!((first.number, first.title.as_str()), (1, "Spec"));
    assert_eq!(first.author_id, Some(9));
    assert_eq!(first.author.as_deref(), Some("robot"));
    assert_eq!(
        (first.created, first.note.as_deref(), first.chars),
        (1000, None, 15)
    );
    assert_eq!(s.revision(d.id, 1).await.unwrap().body, "# Heading\n\ntext");

    let value = serde_json::to_value(s.get(d.id).await).unwrap();
    assert_eq!(value["title"], "Spec");
    assert_eq!(value["revision"], 1);
    assert_eq!(value["parent"], json!(null));
    assert_eq!(value["links"], json!({"issues": [], "initiatives": []}));
    s.store.close().await;
}

#[tokio::test]
async fn titles_and_bodies_have_limits() {
    let s = setup().await;
    let long = "x".repeat(MAX_TITLE_CHARS + 1);
    for bad in ["", "   ", long.as_str()] {
        assert!(is_invalid(s.create(new(bad)).await.unwrap_err()), "{bad:?}");
    }
    s.create(new(&"é".repeat(MAX_TITLE_CHARS))).await.unwrap();
    let too_long = NewDocument {
        body: Some("y".repeat(MAX_BODY_CHARS + 1)),
        ..new("big")
    };
    assert!(is_invalid(s.create(too_long).await.unwrap_err()));
    let biggest = NewDocument {
        body: Some("ü".repeat(MAX_BODY_CHARS)),
        ..new("biggest")
    };
    let id = s.create(biggest).await.unwrap().document.id;
    assert!(is_invalid(
        s.update(id, body(&"z".repeat(MAX_BODY_CHARS + 1)))
            .await
            .unwrap_err()
    ));
    assert!(is_invalid(
        s.update(
            id,
            DocumentPatch {
                title: Some(" ".into()),
                ..DocumentPatch::default()
            }
        )
        .await
        .unwrap_err()
    ));
    assert_eq!(
        s.revisions(id).await.len(),
        1,
        "refused edits leave no revision"
    );
    s.store.close().await;
}

#[tokio::test]
async fn unknown_and_foreign_documents_are_not_found() {
    let s = setup().await;
    let id = s.doc("mine").await;
    assert!(is_not_found(
        s.store
            .read(|c| store::get_document(c, "TRK", 999))
            .await
            .unwrap_err()
    ));
    assert!(is_not_found(
        s.store
            .read(move |c| store::get_document(c, "OTH", id))
            .await
            .unwrap_err()
    ));
    assert!(is_not_found(
        s.store
            .read(move |c| store::get_document(c, "NOPE", id))
            .await
            .unwrap_err()
    ));
    assert!(is_not_found(s.update(999, body("x")).await.unwrap_err()));
    assert!(is_not_found(s.archive(999).await.unwrap_err()));
    assert!(is_not_found(s.revision(id, 2).await.unwrap_err()));
    assert!(is_not_found(s.revision(999, 1).await.unwrap_err()));
    s.store.close().await;
}

// ---- revisions ----

#[tokio::test]
async fn edits_append_revisions_and_keep_the_whole_history() {
    let s = setup().await;
    let id = s
        .create(NewDocument {
            body: Some("one".into()),
            ..new("First")
        })
        .await
        .unwrap()
        .document
        .id;
    let second = s.update(id, body("two")).await.unwrap();
    assert_eq!(
        (second.document.revision, second.document.body.as_str()),
        (2, "two")
    );
    assert_eq!(
        (second.document.created, second.document.updated),
        (1000, 1010)
    );
    let third = s
        .update(
            id,
            DocumentPatch {
                title: Some("Second title".into()),
                body: Some("three".into()),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (third.document.revision, third.document.title.as_str()),
        (3, "Second title")
    );
    let fourth = s
        .update(
            id,
            DocumentPatch {
                title: Some("Third title".into()),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (fourth.document.revision, fourth.document.body.as_str()),
        (4, "three"),
        "a title-only edit keeps the body and still makes a revision"
    );

    let history = s.revisions(id).await;
    let numbers: Vec<i64> = history.iter().map(|r| r.number).collect();
    assert_eq!(numbers, [4, 3, 2, 1], "newest first");
    let titles: Vec<&str> = history.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["Third title", "Second title", "First", "First"]);
    for (number, expect) in [(1, "one"), (2, "two"), (3, "three"), (4, "three")] {
        assert_eq!(s.revision(id, number).await.unwrap().body, expect);
    }
    s.store.close().await;
}

#[tokio::test]
async fn an_edit_that_changes_nothing_writes_no_revision_and_no_event() {
    let s = setup().await;
    let id = s
        .create(NewDocument {
            body: Some("same".into()),
            ..new("Same")
        })
        .await
        .unwrap()
        .document
        .id;
    let events = s.event_count().await;
    for patch in [
        DocumentPatch::default(),
        body("same"),
        DocumentPatch {
            title: Some("  Same ".into()),
            body: Some("same".into()),
            parent: Some(None),
            base_revision: Some(1),
        },
    ] {
        let got = s.update(id, patch).await.unwrap();
        assert_eq!((got.document.revision, got.document.updated), (1, 1000));
    }
    assert_eq!(s.revisions(id).await.len(), 1);
    assert_eq!(s.event_count().await, events);
    s.store.close().await;
}

#[tokio::test]
async fn base_revision_stops_two_editors_overwriting_each_other() {
    let s = setup().await;
    let id = s.doc("Shared").await;
    s.update(
        id,
        DocumentPatch {
            body: Some("a".into()),
            base_revision: Some(1),
            ..DocumentPatch::default()
        },
    )
    .await
    .unwrap();
    // A second editor still on revision 1.
    let err = s
        .update(
            id,
            DocumentPatch {
                body: Some("b".into()),
                base_revision: Some(1),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(is_conflict(err));
    let err = s
        .update(
            id,
            DocumentPatch {
                body: Some("b".into()),
                base_revision: Some(7),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        is_conflict(err),
        "a revision from the future is a conflict too"
    );
    let current = s.get(id).await.document;
    assert_eq!((current.revision, current.body.as_str()), (2, "a"));
    assert_eq!(s.revisions(id).await.len(), 2);

    // The right base goes through; no base means last write wins.
    s.update(
        id,
        DocumentPatch {
            body: Some("c".into()),
            base_revision: Some(2),
            ..DocumentPatch::default()
        },
    )
    .await
    .unwrap();
    let last = s.update(id, body("d")).await.unwrap();
    assert_eq!(
        (last.document.revision, last.document.body.as_str()),
        (4, "d")
    );
    s.store.close().await;
}

#[tokio::test]
async fn restoring_appends_a_new_revision_and_never_rewrites_history() {
    let s = setup().await;
    let id = s
        .create(NewDocument {
            body: Some("v1".into()),
            ..new("Title 1")
        })
        .await
        .unwrap()
        .document
        .id;
    s.update(
        id,
        DocumentPatch {
            title: Some("Title 2".into()),
            body: Some("v2".into()),
            ..DocumentPatch::default()
        },
    )
    .await
    .unwrap();
    s.update(id, body("v3")).await.unwrap();
    let before = s.revisions(id).await;

    let restored = s.restore(id, 1, None).await.unwrap();
    assert_eq!(
        (
            restored.document.revision,
            restored.document.title.as_str(),
            restored.document.body.as_str()
        ),
        (4, "Title 1", "v1")
    );
    let after = s.revisions(id).await;
    assert_eq!(after.len(), 4);
    assert_eq!(&after[1..], &before[..], "revisions 1-3 are untouched");
    assert_eq!(after[0].note.as_deref(), Some("restored from 1"));
    assert_eq!(after[0].author_id, Some(9));
    assert_eq!(s.revision(id, 4).await.unwrap().body, "v1");
    assert_eq!(s.revision(id, 3).await.unwrap().body, "v3");

    // Restoring what the document already says is a no-op.
    let events = s.event_count().await;
    let same = s.restore(id, 4, None).await.unwrap();
    assert_eq!(same.document.revision, 4);
    let same = s.restore(id, 1, None).await.unwrap();
    assert_eq!(same.document.revision, 4);
    assert_eq!(s.event_count().await, events);

    assert!(is_not_found(s.restore(id, 9, None).await.unwrap_err()));
    assert!(is_conflict(s.restore(id, 2, Some(3)).await.unwrap_err()));
    let ok = s.restore(id, 2, Some(4)).await.unwrap();
    assert_eq!((ok.document.revision, ok.document.body.as_str()), (5, "v2"));
    s.store.close().await;
}

// ---- parents ----

#[tokio::test]
async fn children_ancestors_and_the_tree_order() {
    let s = setup().await;
    let a = s.doc("A").await;
    let b = s.doc("B").await;
    let a1 = s.child("A1", a).await;
    let a2 = s.child("A2", a).await;
    let a11 = s.child("A1.1", a1).await;
    s.child("B1", b).await;

    let detail = s.get(a11).await;
    let path: Vec<(i64, &str)> = detail
        .ancestors
        .iter()
        .map(|r| (r.id, r.title.as_str()))
        .collect();
    assert_eq!(path, [(a, "A"), (a1, "A1")], "top-level ancestor first");
    assert_eq!(detail.document.parent, Some(a1));
    let kids: Vec<i64> = s.get(a).await.children.iter().map(|r| r.id).collect();
    assert_eq!(kids, [a1, a2]);

    let tree = s.titles().await;
    let expect: Vec<(usize, &str)> = vec![
        (0, "A"),
        (1, "A1"),
        (2, "A1.1"),
        (1, "A2"),
        (0, "B"),
        (1, "B1"),
    ];
    assert_eq!(
        tree.iter()
            .map(|(d, t)| (*d, t.as_str()))
            .collect::<Vec<_>>(),
        expect
    );
    s.store.close().await;
}

#[tokio::test]
async fn parent_rules_same_project_no_cycles_and_a_depth_limit() {
    let s = setup().await;
    let theirs = s.create_in("OTH", new("theirs")).await.unwrap().document.id;
    let a = s.doc("A").await;
    let b = s.child("B", a).await;
    let c = s.child("C", b).await;

    assert!(
        is_invalid(s.create(under("x", theirs)).await.unwrap_err()),
        "other project"
    );
    assert!(
        is_invalid(s.create(under("x", 999)).await.unwrap_err()),
        "unknown parent"
    );
    let err = s
        .update(
            a,
            DocumentPatch {
                parent: Some(Some(theirs)),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(is_invalid(err));

    // Cycles: itself, a child, a descendant.
    for (doc, parent) in [(a, a), (a, b), (a, c), (b, c)] {
        let err = s
            .update(
                doc,
                DocumentPatch {
                    parent: Some(Some(parent)),
                    ..DocumentPatch::default()
                },
            )
            .await
            .unwrap_err();
        assert!(is_conflict(err), "{doc} under {parent}");
        let err = s
            .mv(
                doc,
                DocumentMove {
                    parent: Some(Some(parent)),
                    ..DocumentMove::default()
                },
            )
            .await
            .unwrap_err();
        assert!(is_conflict(err), "move {doc} under {parent}");
    }
    assert_eq!(s.get(a).await.document.parent, None);

    // Back to the top level, and under another parent.
    let top = s
        .update(
            c,
            DocumentPatch {
                parent: Some(None),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(top.document.parent, None);
    assert_eq!(
        top.document.revision, 1,
        "a parent change is not a content revision"
    );
    let moved = s
        .update(
            c,
            DocumentPatch {
                parent: Some(Some(a)),
                ..DocumentPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.document.parent, Some(a));

    // Depth: MAX_DEPTH levels are fine, one more is refused.
    let mut chain = vec![s.doc("L1").await];
    for level in 2..=MAX_DEPTH {
        let parent = *chain.last().unwrap();
        chain.push(s.child(&format!("L{level}"), parent).await);
    }
    let deepest = *chain.last().unwrap();
    let err = s.create(under("L9", deepest)).await.unwrap_err();
    assert!(is_invalid(err), "level {} is too deep", MAX_DEPTH + 1);
    // Moving a subtree under the chain counts its height.
    let sub = s.doc("Sub").await;
    s.child("Sub1", sub).await;
    let err = s
        .mv(
            sub,
            DocumentMove {
                parent: Some(Some(chain[MAX_DEPTH as usize - 2])),
                ..DocumentMove::default()
            },
        )
        .await
        .unwrap_err();
    assert!(is_invalid(err));
    s.mv(
        sub,
        DocumentMove {
            parent: Some(Some(chain[MAX_DEPTH as usize - 3])),
            ..DocumentMove::default()
        },
    )
    .await
    .unwrap();
    s.store.close().await;
}

// ---- ordering ----

fn at(parent: Option<Option<i64>>) -> DocumentMove {
    DocumentMove {
        parent,
        ..DocumentMove::default()
    }
}

#[tokio::test]
async fn moving_orders_siblings_before_after_position_and_end() {
    let s = setup().await;
    let a = s.doc("A").await;
    let b = s.doc("B").await;
    let c = s.doc("C").await;
    let d = s.doc("D").await;
    let order = |s: &S| {
        let list = s.store.clone();
        async move {
            list.read(|c| store::list_documents(c, "TRK", false))
                .await
                .unwrap()
                .into_iter()
                .map(|d| d.title)
                .collect::<Vec<_>>()
                .join("")
        }
    };
    assert_eq!(order(&s).await, "ABCD");

    s.mv(
        d,
        DocumentMove {
            before: Some(a),
            ..at(None)
        },
    )
    .await
    .unwrap();
    assert_eq!(order(&s).await, "DABC");
    s.mv(
        d,
        DocumentMove {
            after: Some(b),
            ..at(None)
        },
    )
    .await
    .unwrap();
    assert_eq!(order(&s).await, "ABDC");
    s.mv(a, at(None)).await.unwrap();
    assert_eq!(order(&s).await, "BDCA", "no target means the end");
    s.mv(
        c,
        DocumentMove {
            position: Some(1.5),
            ..at(None)
        },
    )
    .await
    .unwrap();
    assert_eq!(order(&s).await, "CBDA");

    // Many insertions into the same gap keep the order (renumbering kicks in).
    let mut expect = String::from("CBDA");
    for n in 0..60 {
        let id = s.doc(&format!("n{n}")).await;
        s.mv(
            id,
            DocumentMove {
                after: Some(c),
                ..at(None)
            },
        )
        .await
        .unwrap();
        expect.insert_str(1, &format!("n{n}"));
    }
    assert_eq!(order(&s).await, expect);

    let err = s
        .mv(
            a,
            DocumentMove {
                before: Some(a),
                ..at(None)
            },
        )
        .await
        .unwrap_err();
    assert!(is_invalid(err), "relative to itself");
    let err = s
        .mv(
            a,
            DocumentMove {
                before: Some(b),
                after: Some(c),
                ..at(None)
            },
        )
        .await
        .unwrap_err();
    assert!(is_invalid(err), "two anchors");
    let err = s
        .mv(
            a,
            DocumentMove {
                position: Some(f64::NAN),
                ..at(None)
            },
        )
        .await
        .unwrap_err();
    assert!(is_invalid(err));
    let child = s.child("child", b).await;
    let err = s
        .mv(
            a,
            DocumentMove {
                before: Some(child),
                ..at(None)
            },
        )
        .await
        .unwrap_err();
    assert!(is_invalid(err), "the anchor must be a sibling");
    s.store.close().await;
}

#[tokio::test]
async fn moving_between_parents_and_a_move_that_changes_nothing() {
    let s = setup().await;
    let a = s.doc("A").await;
    let b = s.doc("B").await;
    let x = s.child("x", a).await;
    let y = s.child("y", a).await;
    s.child("z", b).await;

    let moved = s
        .mv(
            y,
            DocumentMove {
                parent: Some(Some(b)),
                ..DocumentMove::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.document.parent, Some(b));
    assert_eq!(moved.document.revision, 1);
    let kids: Vec<String> = s
        .get(b)
        .await
        .children
        .into_iter()
        .map(|r| r.title)
        .collect();
    assert_eq!(kids, ["z", "y"], "lands last by default");
    let first = s
        .mv(
            y,
            DocumentMove {
                before: Some(s.get(b).await.children[0].id),
                ..DocumentMove::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.document.parent, Some(b));
    s.mv(
        y,
        DocumentMove {
            parent: Some(None),
            ..DocumentMove::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.get(y).await.document.parent, None);

    // Where it already is: nothing written.
    let events = s.event_count().await;
    s.mv(x, DocumentMove::default()).await.unwrap();
    s.mv(
        x,
        DocumentMove {
            parent: Some(Some(a)),
            ..DocumentMove::default()
        },
    )
    .await
    .unwrap();
    s.mv(
        a,
        DocumentMove {
            before: Some(b),
            ..DocumentMove::default()
        },
    )
    .await
    .unwrap();
    s.mv(
        b,
        DocumentMove {
            after: Some(a),
            ..DocumentMove::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.event_count().await, events);
    s.store.close().await;
}

// ---- archiving ----

#[tokio::test]
async fn archiving_is_a_soft_delete_refused_while_children_live() {
    let s = setup().await;
    let a = s.doc("A").await;
    let b = s.child("B", a).await;
    let c = s.doc("C").await;

    let err = s.archive(a).await.unwrap_err();
    assert!(is_conflict(err), "A still has a child");
    s.archive(b).await.unwrap();
    let archived = s.archive(a).await.unwrap();
    assert!(archived.document.archived);

    let visible: Vec<i64> = s.list(false).await.iter().map(|d| d.id).collect();
    assert_eq!(visible, [c], "archived documents are hidden from lists");
    let everything = s.list(true).await;
    assert_eq!(everything.len(), 3);
    assert!(everything.iter().filter(|d| d.archived).count() == 2);
    assert!(
        s.get(a).await.children.is_empty(),
        "archived children are not listed"
    );
    s.revision(a, 1).await.unwrap();

    // Nothing changes an archived document, nothing goes under it.
    assert!(is_conflict(s.update(a, body("x")).await.unwrap_err()));
    assert!(is_conflict(
        s.mv(a, DocumentMove::default()).await.unwrap_err()
    ));
    assert!(is_conflict(s.restore(a, 1, None).await.unwrap_err()));
    assert!(is_conflict(
        s.link(a, DocumentTarget::Issue(1)).await.unwrap_err()
    ));
    assert!(is_conflict(s.create(under("late", a)).await.unwrap_err()));
    assert!(is_conflict(
        s.mv(
            c,
            DocumentMove {
                parent: Some(Some(a)),
                ..DocumentMove::default()
            }
        )
        .await
        .unwrap_err()
    ));

    // The child cannot come back under an archived parent; the parent first.
    assert!(is_conflict(s.unarchive(b).await.unwrap_err()));
    s.unarchive(a).await.unwrap();
    let back = s.unarchive(b).await.unwrap();
    assert!(!back.document.archived);
    assert_eq!(s.get(a).await.children.len(), 1);

    let events = s.event_count().await;
    s.archive(c).await.unwrap();
    s.archive(c).await.unwrap();
    s.unarchive(a).await.unwrap();
    assert_eq!(
        s.event_count().await,
        events + 1,
        "repeating changes nothing"
    );
    s.store.close().await;
}

// ---- links ----

#[tokio::test]
async fn links_show_on_both_sides_and_only_within_the_project() {
    let s = setup().await;
    let a = s.doc("Design").await;
    let b = s.doc("Notes").await;

    let to_issue = s.link(a, DocumentTarget::Issue(1)).await.unwrap();
    assert_eq!((to_issue.kind, to_issue.document_id), ("issue", a));
    let issue_ref = to_issue.issue.clone().unwrap();
    assert_eq!(
        (
            issue_ref.number,
            issue_ref.identifier.as_str(),
            issue_ref.title.as_str()
        ),
        (1, "TRK-1", "one")
    );
    assert!(to_issue.initiative.is_none());
    let to_initiative = s.link(a, DocumentTarget::Initiative(1)).await.unwrap();
    assert_eq!(to_initiative.kind, "initiative");
    assert_eq!(
        to_initiative.initiative.clone().unwrap().name,
        "TRK initiative"
    );
    s.link(b, DocumentTarget::Issue(1)).await.unwrap();

    // The document's side.
    let detail = s.get(a).await;
    assert_eq!(detail.links.issues.len(), 1);
    assert_eq!(detail.links.issues[0].identifier, "TRK-1");
    assert_eq!(detail.links.initiatives.len(), 1);
    assert_eq!(detail.links.initiatives[0].id, 1);

    // The issue's and the initiative's side.
    let issue = s
        .store
        .read(|c| issues::get_issue(c, "TRK", 1))
        .await
        .unwrap();
    let docs: Vec<(i64, &str)> = issue
        .documents
        .iter()
        .map(|d| (d.id, d.title.as_str()))
        .collect();
    assert_eq!(docs, [(a, "Design"), (b, "Notes")]);
    assert_eq!(issue.documents[0].link_id, to_issue.id);
    let other_issue = s
        .store
        .read(|c| issues::get_issue(c, "TRK", 2))
        .await
        .unwrap();
    assert!(other_issue.documents.is_empty());
    let initiative = s
        .store
        .read(|c| initiatives::get_initiative(c, 5000, "TRK", 1, None, None))
        .await
        .unwrap();
    assert_eq!(initiative.documents.len(), 1);
    assert_eq!(initiative.documents[0].id, a);
    let json = serde_json::to_value(&initiative).unwrap();
    assert_eq!(json["documents"][0]["title"], "Design");
    let json = serde_json::to_value(&issue).unwrap();
    assert_eq!(json["documents"][1]["id"], b);

    // Duplicates, other projects, unknown targets.
    assert!(is_conflict(
        s.link(a, DocumentTarget::Issue(1)).await.unwrap_err()
    ));
    assert!(is_conflict(
        s.link(a, DocumentTarget::Initiative(1)).await.unwrap_err()
    ));
    assert!(
        is_invalid(s.link(a, DocumentTarget::Initiative(2)).await.unwrap_err()),
        "OTH's initiative"
    );
    assert!(is_invalid(
        s.link(a, DocumentTarget::Issue(3)).await.unwrap_err()
    ));
    assert!(is_invalid(
        s.link(a, DocumentTarget::Initiative(99)).await.unwrap_err()
    ));
    assert!(is_not_found(
        s.link(999, DocumentTarget::Issue(1)).await.unwrap_err()
    ));
    s.store.close().await;
}

#[tokio::test]
async fn unlinking_and_archiving_change_what_the_other_side_shows() {
    let s = setup().await;
    let a = s.doc("A").await;
    let b = s.doc("B").await;
    let la = s.link(a, DocumentTarget::Issue(2)).await.unwrap();
    s.link(b, DocumentTarget::Issue(2)).await.unwrap();

    // A link belongs to its document.
    assert!(is_not_found(s.unlink(b, la.id).await.unwrap_err()));
    assert!(is_not_found(s.unlink(a, 999).await.unwrap_err()));
    s.unlink(a, la.id).await.unwrap();
    assert!(s.get(a).await.links.issues.is_empty());
    let shown = |s: &S| {
        let store = s.store.clone();
        async move {
            store
                .read(|c| issues::get_issue(c, "TRK", 2))
                .await
                .unwrap()
                .documents
        }
    };
    assert_eq!(shown(&s).await.len(), 1);

    s.archive(b).await.unwrap();
    assert!(
        shown(&s).await.is_empty(),
        "an archived document is not listed on the issue"
    );
    s.unarchive(b).await.unwrap();
    assert_eq!(shown(&s).await.len(), 1, "its link survived");
    // The link can be linked again after it was removed.
    s.link(a, DocumentTarget::Issue(2)).await.unwrap();
    s.store.close().await;
}

#[tokio::test]
async fn deleting_an_initiative_drops_its_document_links_with_events() {
    let s = setup().await;
    let a = s.doc("A").await;
    let b = s.doc("B").await;
    s.link(a, DocumentTarget::Initiative(1)).await.unwrap();
    s.link(b, DocumentTarget::Initiative(1)).await.unwrap();
    s.link(b, DocumentTarget::Issue(1)).await.unwrap();

    let before = s.log("").await.len();
    s.store
        .write(|tx| initiatives::delete_initiative(tx, 9000, "TRK", 1))
        .await
        .unwrap();
    assert!(s.get(a).await.links.initiatives.is_empty());
    assert_eq!(s.get(b).await.links.initiatives.len(), 0);
    assert_eq!(s.get(b).await.links.issues.len(), 1, "issue links stay");
    let log = s.log("").await;
    let fresh = &log[before..];
    let kinds: Vec<&str> = fresh.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "document.link_removed",
            "document.link_removed",
            "initiative.deleted"
        ]
    );
    assert_eq!(fresh[0].entity_id, a);
    assert_eq!(fresh[1].entity_id, b);
    assert_eq!(
        fresh[0].payload["target"],
        json!({"kind": "initiative", "id": 1})
    );
    s.store.close().await;
}

#[tokio::test]
async fn remove_link_to_finds_the_link_by_its_target() {
    let s = setup().await;
    let a = s.doc("A").await;
    s.link(a, DocumentTarget::Issue(1)).await.unwrap();
    s.link(a, DocumentTarget::Initiative(1)).await.unwrap();
    for target in [DocumentTarget::Issue(1), DocumentTarget::Initiative(1)] {
        s.store
            .write(move |tx| store::remove_link_to(tx, 9000, &ROBOT, "TRK", a, target))
            .await
            .unwrap();
        let again = s
            .store
            .write(move |tx| store::remove_link_to(tx, 9000, &ROBOT, "TRK", a, target))
            .await
            .unwrap_err();
        assert!(is_not_found(again));
    }
    assert_eq!(s.log("document.link_removed").await.len(), 2);
    s.store.close().await;
}

// ---- events ----

#[tokio::test]
async fn events_carry_the_actor_the_project_and_never_a_body() {
    let s = setup().await;
    let secret = "SECRET-BODY-TEXT-0451";
    let parent = s.doc("Parent").await;
    let id = s
        .create(NewDocument {
            title: "Doc".into(),
            body: Some(format!("{secret} one")),
            parent: Some(parent),
        })
        .await
        .unwrap()
        .document
        .id;
    s.update(id, body(&format!("{secret} two"))).await.unwrap();
    s.update(
        id,
        DocumentPatch {
            title: Some("Doc 2".into()),
            body: Some(format!("{secret} three")),
            ..DocumentPatch::default()
        },
    )
    .await
    .unwrap();
    s.update(
        id,
        DocumentPatch {
            parent: Some(None),
            ..DocumentPatch::default()
        },
    )
    .await
    .unwrap();
    s.mv(
        id,
        DocumentMove {
            before: Some(parent),
            ..DocumentMove::default()
        },
    )
    .await
    .unwrap();
    s.restore(id, 1, None).await.unwrap();
    s.link(id, DocumentTarget::Issue(2)).await.unwrap();
    let initiative = s.link(id, DocumentTarget::Initiative(1)).await.unwrap();
    s.unlink(id, initiative.id).await.unwrap();
    s.archive(id).await.unwrap();
    s.unarchive(id).await.unwrap();

    let log = s.log("document.").await;
    let kinds: Vec<&str> = log.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "document.created",
            "document.created",
            "document.updated",
            "document.updated",
            "document.moved",
            "document.moved",
            "document.restored",
            "document.link_added",
            "document.link_added",
            "document.link_removed",
            "document.archived",
            "document.unarchived",
        ]
    );
    for event in &log {
        assert_eq!(event.actor, Some(9), "{}", event.kind);
        assert_eq!(event.project.as_deref(), Some("TRK"));
        assert_eq!(event.entity_kind, "document");
        assert!(
            !event.payload.to_string().contains(secret),
            "{} carries a body: {}",
            event.kind,
            event.payload
        );
    }
    let by_kind = |kind: &str| -> Vec<&Event> { log.iter().filter(|e| e.kind == kind).collect() };
    let created = by_kind("document.created");
    assert_eq!(
        created[0].payload,
        json!({"title": "Parent", "parent": null, "revision": 1, "chars": 0})
    );
    assert_eq!(created[1].entity_id, id);
    assert_eq!(
        created[1].payload,
        json!({"title": "Doc", "parent": parent, "revision": 1, "chars": 25})
    );
    let updated = by_kind("document.updated");
    assert_eq!(
        updated[0].payload,
        json!({"revision": 2, "body_changed": true, "chars": 25, "changes": {}})
    );
    assert_eq!(
        updated[1].payload,
        json!({"revision": 3, "body_changed": true, "chars": 27, "changes": {"title": {"from": "Doc", "to": "Doc 2"}}})
    );
    // A parent-only patch is a move, with no content event.
    assert_eq!(updated.len(), 2);
    let moved = by_kind("document.moved");
    assert_eq!(
        (
            moved[0].payload["from_parent"].clone(),
            moved[0].payload["to_parent"].clone()
        ),
        (json!(parent), json!(null))
    );
    assert!(moved[1].payload["position"].is_number());
    let restored = by_kind("document.restored");
    assert_eq!(
        restored[0].payload,
        json!({"from_revision": 1, "revision": 4, "title": "Doc", "chars": 25})
    );
    let added = by_kind("document.link_added");
    assert_eq!(
        added[0].payload["target"],
        json!({"kind": "issue", "number": 2})
    );
    assert_eq!(
        added[1].payload["target"],
        json!({"kind": "initiative", "id": 1})
    );
    assert_eq!(
        by_kind("document.archived")[0].payload,
        json!({"title": "Doc"})
    );
    s.store.close().await;
}

#[tokio::test]
async fn a_failed_write_leaves_no_document_revision_link_or_event() {
    let s = setup().await;
    let events = s.event_count().await;
    let err = s
        .store
        .write(|tx| {
            let made = store::create_document(tx, 5000, &ROBOT, "TRK", new("Doomed"))?;
            store::update_document(tx, 5001, &ROBOT, "TRK", made.document.id, body("more"))?;
            store::add_link(
                tx,
                5002,
                &ROBOT,
                "TRK",
                made.document.id,
                DocumentTarget::Issue(1),
            )?;
            // The last step fails: the whole transaction goes.
            store::add_link(
                tx,
                5003,
                &ROBOT,
                "TRK",
                made.document.id,
                DocumentTarget::Issue(1),
            )
        })
        .await
        .unwrap_err();
    assert!(is_conflict(err));
    assert!(s.list(true).await.is_empty());
    assert_eq!(s.event_count().await, events);
    let counts: (i64, i64) = s
        .store
        .read(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM document_revision", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM document_link", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    s.store.close().await;
}

#[tokio::test]
async fn a_change_and_its_event_are_written_together_with_the_revision() {
    let s = setup().await;
    let id = s.doc("D").await;
    s.update(id, body("x")).await.unwrap();
    s.restore(id, 1, None).await.unwrap();
    // Every content event has exactly one revision with that number, and back.
    let log = s.log("document.").await;
    let numbers: Vec<i64> = log
        .iter()
        .filter_map(|e| e.payload["revision"].as_i64())
        .collect();
    let stored: Vec<i64> = s
        .revisions(id)
        .await
        .iter()
        .rev()
        .map(|r| r.number)
        .collect();
    assert_eq!(numbers, stored);
    s.store.close().await;
}
