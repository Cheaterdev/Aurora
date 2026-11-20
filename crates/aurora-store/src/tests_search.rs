//! Full-text search: triggers, ranking, query parsing, paging, performance.

use std::time::Instant;

use crate::comments::{self, NewComment};
use crate::documents::{DocumentPatch, NewDocument};
use crate::issues::{IssuePatch, NewIssue};
use crate::projects::Rejection;
use crate::search::{
    self, Hit, Kind, MAX_QUERY_CHARS, MAX_TOKENS, SearchQuery, SearchResults, parse_query,
    search_check, search_rebuild,
};
use crate::tests_support::{documents, issues, projects, test_actor};
use crate::{Store, StoreError};

struct T {
    _dir: tempfile::TempDir,
    store: Store,
}

/// Projects TRK and OTH, each with a Task type and no issues.
async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .write(|tx| {
            for key in ["TRK", "OTH"] {
                projects::create_project(tx, 1, key, key, None)?;
                projects::create_issue_type(tx, 1, key, "Task")?;
            }
            Ok(())
        })
        .await
        .unwrap();
    T { _dir: dir, store }
}

fn q(text: &str) -> SearchQuery {
    SearchQuery {
        q: text.into(),
        ..SearchQuery::default()
    }
}

impl T {
    async fn issue(&self, key: &'static str, title: &str, description: &str) -> i64 {
        let (title, description) = (title.to_string(), description.to_string());
        self.store
            .write(move |tx| {
                let made = issues::create_issue(
                    tx,
                    1,
                    key,
                    NewIssue {
                        title,
                        description: Some(description),
                        ..NewIssue::default()
                    },
                )?;
                Ok(made.issue.number)
            })
            .await
            .unwrap()
    }

    async fn comment(&self, key: &'static str, number: i64, body: &str) -> i64 {
        let body = body.to_string();
        self.store
            .write(move |tx| {
                let actor = test_actor(tx);
                let made = comments::add_comment(
                    tx,
                    2,
                    &actor,
                    key,
                    number,
                    NewComment { body, parent: None },
                )?;
                Ok(made.id)
            })
            .await
            .unwrap()
    }

    async fn document(&self, key: &'static str, title: &str, body: &str) -> i64 {
        let (title, body) = (title.to_string(), body.to_string());
        self.store
            .write(move |tx| {
                let made = documents::create_document(
                    tx,
                    1,
                    key,
                    NewDocument {
                        title,
                        body: Some(body),
                        parent: None,
                    },
                )?;
                Ok(made.document.id)
            })
            .await
            .unwrap()
    }

    async fn run(&self, query: SearchQuery) -> SearchResults {
        self.store
            .read(move |c| search::search(c, &query))
            .await
            .unwrap()
    }

    async fn names(&self, text: &str) -> Vec<String> {
        names(&self.run(q(text)).await)
    }

    async fn check(&self) {
        let check = self.store.read(search_check).await.unwrap();
        assert!(check.is_consistent(), "{check:?}");
    }
}

fn name(hit: &Hit) -> String {
    match hit.kind {
        Kind::Issue => hit.issue.as_ref().unwrap().identifier.clone(),
        Kind::Comment => format!(
            "{}#c{}",
            hit.issue.as_ref().unwrap().identifier,
            hit.comment_id.unwrap()
        ),
        Kind::Document => format!("{}:doc{}", hit.project, hit.document.as_ref().unwrap().id),
    }
}

fn names(results: &SearchResults) -> Vec<String> {
    results.hits.iter().map(name).collect()
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

// ---- triggers keep the index right ----

#[tokio::test]
async fn issue_changes_reach_the_index() {
    let t = setup().await;
    let n = t.issue("TRK", "zebra crossing", "painted stripes").await;
    assert_eq!(t.names("zebra").await, ["TRK-1"]);
    assert_eq!(t.names("stripes").await, ["TRK-1"]);

    t.store
        .write(move |tx| {
            issues::update_issue(
                tx,
                3,
                "TRK",
                n,
                IssuePatch {
                    title: Some("giraffe crossing".into()),
                    ..IssuePatch::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(t.names("zebra").await.is_empty());
    assert_eq!(t.names("giraffe").await, ["TRK-1"]);

    t.store
        .write(move |tx| {
            issues::update_issue(
                tx,
                4,
                "TRK",
                n,
                IssuePatch {
                    description: Some(Some("painted dots".into())),
                    ..IssuePatch::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(t.names("stripes").await.is_empty());
    assert_eq!(t.names("dots").await, ["TRK-1"]);

    t.store
        .write(move |tx| issues::archive_issue(tx, 5, "TRK", n))
        .await
        .unwrap();
    assert!(t.names("giraffe").await.is_empty());
    let archived = t
        .run(SearchQuery {
            include_archived: true,
            ..q("giraffe")
        })
        .await;
    assert_eq!(names(&archived), ["TRK-1"]);
    assert!(archived.hits[0].archived);

    t.store
        .write(move |tx| issues::unarchive_issue(tx, 6, "TRK", n))
        .await
        .unwrap();
    let back = t.run(q("giraffe")).await;
    assert_eq!(names(&back), ["TRK-1"]);
    assert!(!back.hits[0].archived);
    t.check().await;
}

#[tokio::test]
async fn comment_changes_reach_the_index() {
    let t = setup().await;
    let n = t.issue("TRK", "host", "").await;
    let id = t.comment("TRK", n, "a wombat sighting").await;
    let found = t.run(q("wombat")).await;
    assert_eq!(names(&found), [format!("TRK-1#c{id}")]);
    assert_eq!(found.hits[0].kind, Kind::Comment);
    assert_eq!(found.hits[0].comment_id, Some(id));

    t.store
        .write(move |tx| {
            let actor = test_actor(tx);
            comments::edit_comment(tx, 3, &actor, "TRK", id, "a quokka sighting")
        })
        .await
        .unwrap();
    assert!(t.names("wombat").await.is_empty());
    assert_eq!(t.names("quokka").await.len(), 1);

    // The comment follows the archived flag of its issue.
    t.store
        .write(move |tx| issues::archive_issue(tx, 4, "TRK", n))
        .await
        .unwrap();
    assert!(t.names("quokka").await.is_empty());
    let all = t
        .run(SearchQuery {
            include_archived: true,
            ..q("quokka")
        })
        .await;
    assert!(all.hits[0].archived);
    t.store
        .write(move |tx| issues::unarchive_issue(tx, 5, "TRK", n))
        .await
        .unwrap();
    assert_eq!(t.names("quokka").await.len(), 1);

    t.store
        .write(move |tx| {
            let actor = test_actor(tx);
            comments::delete_comment(tx, 6, &actor, "TRK", id)
        })
        .await
        .unwrap();
    assert!(t.names("quokka").await.is_empty());
    t.check().await;
}

#[tokio::test]
async fn document_changes_reach_the_index() {
    let t = setup().await;
    let id = t
        .document("TRK", "Handbook", "first draft about otters")
        .await;
    assert_eq!(t.names("otters").await, [format!("TRK:doc{id}")]);
    assert_eq!(t.names("handbook").await.len(), 1);

    t.store
        .write(move |tx| {
            documents::update_document(
                tx,
                2,
                "TRK",
                id,
                DocumentPatch {
                    body: Some("second draft about seals".into()),
                    ..DocumentPatch::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(t.names("otters").await.is_empty());
    assert_eq!(t.names("seals").await.len(), 1);

    // Restoring revision 1 brings the old text back.
    t.store
        .write(move |tx| documents::restore_revision(tx, 3, "TRK", id, 1))
        .await
        .unwrap();
    assert_eq!(t.names("otters").await.len(), 1);
    assert!(t.names("seals").await.is_empty());

    t.store
        .write(move |tx| documents::archive_document(tx, 4, "TRK", id))
        .await
        .unwrap();
    assert!(t.names("otters").await.is_empty());
    let archived = t
        .run(SearchQuery {
            include_archived: true,
            ..q("otters")
        })
        .await;
    assert!(archived.hits[0].archived);
    t.store
        .write(move |tx| documents::unarchive_document(tx, 5, "TRK", id))
        .await
        .unwrap();
    assert_eq!(t.names("otters").await.len(), 1);
    t.check().await;
}

#[tokio::test]
async fn a_rolled_back_change_leaves_the_index_alone() {
    let t = setup().await;
    t.issue("TRK", "keeper", "").await;
    let before = t.store.read(snapshot).await.unwrap();
    let failed: Result<(), StoreError> = t
        .store
        .write(|tx| {
            issues::create_issue(
                tx,
                9,
                "TRK",
                NewIssue {
                    title: "phantom".into(),
                    ..NewIssue::default()
                },
            )?;
            assert!(search_check(tx)?.indexed.issues == 2);
            Err(StoreError::Closed)
        })
        .await;
    assert!(failed.is_err());
    assert!(t.names("phantom").await.is_empty());
    assert_eq!(t.store.read(snapshot).await.unwrap(), before);
    t.check().await;
}

/// Everything in the index, for comparing two builds of it.
fn snapshot(conn: &rusqlite::Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT rowid, kind, project_id, ref_id, archived, title, body, ident
         FROM search_index ORDER BY rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        usize::try_from(self.0 >> 33).unwrap() % n
    }
}

#[tokio::test]
async fn a_random_sequence_of_changes_keeps_the_index_equal_to_a_rebuild() {
    const WORDS: [&str; 8] = [
        "alpha", "beta", "gamma", "delta", "kappa", "sigma", "omega", "theta",
    ];
    let t = setup().await;
    let mut rng = Rng(7);
    let (mut issues_made, mut comments_made, mut documents_made) = (0_i64, 0_i64, 0_i64);
    for step in 0..400_i64 {
        let words: Vec<&str> = (0..3).map(|_| WORDS[rng.below(8)]).collect();
        let text = words.join(" ");
        let op = rng.below(10);
        let pick = |rng: &mut Rng, n: i64| {
            i64::try_from(rng.below(usize::try_from(n.max(1)).unwrap())).unwrap() + 1
        };
        let (issue, comment, document) = (
            pick(&mut rng, issues_made),
            pick(&mut rng, comments_made),
            pick(&mut rng, documents_made),
        );
        let result: Result<(), StoreError> = t
            .store
            .write(move |tx| {
                let actor = test_actor(tx);
                match op {
                    0 => {
                        issues::create_issue(
                            tx,
                            step,
                            "TRK",
                            NewIssue {
                                title: text.clone(),
                                description: Some(text),
                                ..NewIssue::default()
                            },
                        )?;
                    }
                    1 => {
                        issues::update_issue(
                            tx,
                            step,
                            "TRK",
                            issue,
                            IssuePatch {
                                title: Some(text),
                                ..IssuePatch::default()
                            },
                        )?;
                    }
                    2 => {
                        if issues::archive_issue(tx, step, "TRK", issue).is_err() {
                            issues::unarchive_issue(tx, step, "TRK", issue)?;
                        }
                    }
                    3 => {
                        comments::add_comment(
                            tx,
                            step,
                            &actor,
                            "TRK",
                            issue,
                            NewComment {
                                body: text,
                                parent: None,
                            },
                        )?;
                    }
                    4 => {
                        comments::edit_comment(tx, step, &actor, "TRK", comment, &text)?;
                    }
                    5 => comments::delete_comment(tx, step, &actor, "TRK", comment)?,
                    6 => {
                        documents::create_document(
                            tx,
                            step,
                            "TRK",
                            NewDocument {
                                title: text.clone(),
                                body: Some(text),
                                parent: None,
                            },
                        )?;
                    }
                    7 => {
                        documents::update_document(
                            tx,
                            step,
                            "TRK",
                            document,
                            DocumentPatch {
                                body: Some(text),
                                ..DocumentPatch::default()
                            },
                        )?;
                    }
                    8 => {
                        if documents::archive_document(tx, step, "TRK", document).is_err() {
                            documents::unarchive_document(tx, step, "TRK", document)?;
                        }
                    }
                    _ => {
                        documents::restore_revision(tx, step, "TRK", document, 1)?;
                    }
                }
                Ok(())
            })
            .await;
        if result.is_ok() {
            match op {
                0 => issues_made += 1,
                3 => comments_made += 1,
                6 => documents_made += 1,
                _ => {}
            }
        }
        if step % 20 == 0 {
            t.check().await;
        }
    }
    assert!(issues_made > 10 && comments_made > 10 && documents_made > 10);
    t.check().await;

    let before = t.store.read(snapshot).await.unwrap();
    let rebuilt = t.store.write(search_rebuild).await.unwrap();
    assert!(rebuilt.is_consistent());
    assert_eq!(t.store.read(snapshot).await.unwrap(), before);
}

#[tokio::test]
async fn rebuild_repairs_a_damaged_index() {
    let t = setup().await;
    let n = t.issue("TRK", "intact", "").await;
    t.comment("TRK", n, "also intact").await;
    t.document("TRK", "doc", "intact too").await;
    t.store
        .write(|tx| {
            tx.execute("DELETE FROM search_index WHERE kind = 'comment'", [])?;
            tx.execute(
                "INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
                 VALUES (4000, 'issue', 1, 1000, 0, 'ghost', '', 'X-1')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let broken = t.store.read(search_check).await.unwrap();
    assert!(!broken.is_consistent());
    let fixed = t.store.write(search_rebuild).await.unwrap();
    assert!(fixed.is_consistent());
    assert_eq!(fixed.indexed.comments, 1);
    assert_eq!(t.names("intact").await.len(), 3);
    assert!(t.names("ghost").await.is_empty());
}

#[tokio::test]
async fn a_raw_delete_below_the_store_is_also_indexed() {
    let t = setup().await;
    let n = t.issue("TRK", "short lived", "").await;
    t.comment("TRK", n, "short lived note").await;
    t.store
        .write(|tx| {
            tx.execute("DELETE FROM comment", [])?;
            tx.execute("DELETE FROM issue", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(t.names("short").await.is_empty());
    t.check().await;
}

// ---- matching and ranking ----

#[tokio::test]
async fn a_title_match_ranks_above_a_body_match() {
    let t = setup().await;
    t.issue("TRK", "other thing", "needle needle needle").await;
    t.issue("TRK", "needle", "other thing").await;
    t.issue("TRK", "third", "one needle somewhere in a longer text body")
        .await;
    let found = t.names("needle").await;
    assert_eq!(found[0], "TRK-2");
    assert_eq!(found.len(), 3);
}

#[tokio::test]
async fn words_prefixes_phrases_and_diacritics() {
    let t = setup().await;
    t.issue("TRK", "The compositor flickers", "red apple pie")
        .await;
    t.issue("TRK", "Cafe menu", "apple red tart").await;
    t.issue("TRK", "Na\u{ef}ve caf\u{e9}", "").await;

    assert_eq!(
        t.names("compos").await,
        ["TRK-1"],
        "last word matches as prefix"
    );
    assert!(
        t.names("ositor").await.is_empty(),
        "only prefixes, not suffixes"
    );
    assert_eq!(
        t.names("COMPOSITOR").await,
        ["TRK-1"],
        "case does not matter"
    );
    assert_eq!(
        t.names("red apple").await.len(),
        2,
        "words are ANDed in any order"
    );
    assert_eq!(
        t.names("red apple p").await,
        ["TRK-1"],
        "prefix on the last word"
    );
    assert_eq!(
        t.names("\"red apple\"").await,
        ["TRK-1"],
        "a phrase is in order"
    );
    assert_eq!(t.names("\"apple red\"").await, ["TRK-2"]);
    assert!(t.names("compositor tart").await.is_empty());
    let mut cafes = t.names("cafe").await;
    cafes.sort();
    assert_eq!(cafes, ["TRK-2", "TRK-3"], "diacritics are ignored");
    assert_eq!(t.names("na\u{ef}ve").await, ["TRK-3"]);
    assert_eq!(t.names("naive caf\u{e9}").await, ["TRK-3"]);
}

#[tokio::test]
async fn other_scripts_and_emoji() {
    let t = setup().await;
    t.issue(
        "TRK",
        "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30c6}\u{30b9}\u{30c8}",
        "",
    )
    .await;
    t.issue(
        "TRK",
        "\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43c}\u{438}\u{440}",
        "\u{1f600} smile",
    )
    .await;
    assert_eq!(t.names("\u{65e5}\u{672c}\u{8a9e}").await, ["TRK-1"]);
    assert_eq!(t.names("\u{41f}\u{420}\u{418}\u{412}").await, ["TRK-2"]);
    assert_eq!(t.names("\u{1f600} smile").await, ["TRK-2"]);
}

#[tokio::test]
async fn identifiers_are_found_and_come_first() {
    let t = setup().await;
    for i in 1..=12 {
        let title = if i == 3 {
            "see TRK-12 for details".to_string()
        } else {
            format!("task number {i}")
        };
        t.issue("TRK", &title, "").await;
    }
    t.issue("OTH", "other project", "").await;
    for text in ["TRK-12", "trk-12", "trk 12", "  TRK 12 "] {
        let found = t.run(q(text)).await;
        let first = &found.hits[0];
        assert_eq!(name(first), "TRK-12", "{text}");
        assert!(first.exact, "{text}");
        let mut unique = names(&found);
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), found.hits.len(), "no repeats: {text}");
        assert!(
            names(&found).contains(&"TRK-3".to_string()),
            "the mention is found by words too"
        );
        assert_eq!(found.total, i64::try_from(found.hits.len()).unwrap());
    }
    // Filters apply to the identifier hit as well.
    let other = t
        .run(SearchQuery {
            project: Some("OTH".into()),
            ..q("TRK-12")
        })
        .await;
    assert!(other.hits.is_empty());
    let comments_only = t
        .run(SearchQuery {
            kinds: Some(vec![Kind::Comment]),
            ..q("TRK-12")
        })
        .await;
    assert!(comments_only.hits.is_empty());
    // Not an issue number: just words.
    assert!(t.names("TRK-99").await.is_empty());
    // The identifier alone matches by words: every issue of a project says TRK.
    assert!(t.names("trk").await.len() >= 12);
}

#[tokio::test]
async fn filters_by_project_and_kind() {
    let t = setup().await;
    let a = t.issue("TRK", "shared word", "").await;
    t.comment("TRK", a, "shared word").await;
    t.document("TRK", "shared word", "").await;
    let b = t.issue("OTH", "shared word", "").await;
    t.comment("OTH", b, "shared word").await;

    assert_eq!(t.run(q("shared")).await.total, 5);
    let trk = t
        .run(SearchQuery {
            project: Some("TRK".into()),
            ..q("shared")
        })
        .await;
    assert_eq!(trk.total, 3);
    assert!(trk.hits.iter().all(|h| h.project == "TRK"));

    let docs = t
        .run(SearchQuery {
            kinds: Some(vec![Kind::Document]),
            ..q("shared")
        })
        .await;
    assert_eq!(docs.hits.len(), 1);
    assert_eq!(docs.hits[0].kind, Kind::Document);
    let two = t
        .run(SearchQuery {
            kinds: Some(vec![Kind::Issue, Kind::Comment]),
            project: Some("OTH".into()),
            ..q("shared")
        })
        .await;
    assert_eq!(two.total, 2);

    let missing = t
        .store
        .read(|c| {
            search::search(
                c,
                &SearchQuery {
                    project: Some("NOPE".into()),
                    ..q("shared")
                },
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    let none = t
        .store
        .read(|c| {
            search::search(
                c,
                &SearchQuery {
                    kinds: Some(vec![]),
                    ..q("shared")
                },
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(none), Rejection::Invalid(_)));
}

#[tokio::test]
async fn paging_is_stable_and_complete() {
    let t = setup().await;
    for i in 1..=25 {
        t.issue("TRK", &format!("common item {i}"), "").await;
    }
    let mut seen = Vec::new();
    for (offset, expected) in [(0, 10), (10, 10), (20, 5), (30, 0)] {
        let page = t
            .run(SearchQuery {
                limit: Some(10),
                offset: Some(offset),
                ..q("common")
            })
            .await;
        assert_eq!(page.total, 25);
        assert!(!page.total_capped);
        assert_eq!(page.hits.len(), expected, "offset {offset}");
        seen.extend(names(&page));
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 25);
    // Equal ranks are ordered by id.
    let by_id: Vec<String> = (1..=25).map(|i| format!("TRK-{i}")).collect();
    let mut sorted = seen.clone();
    sorted.sort_by_key(|n| n.trim_start_matches("TRK-").parse::<i64>().unwrap());
    assert_eq!(sorted, by_id);

    // Paging past an identifier hit neither repeats nor drops a row.
    let mut with_exact = Vec::new();
    for offset in [0, 10, 20] {
        let page = t
            .run(SearchQuery {
                limit: Some(10),
                offset: Some(offset),
                ..q("trk 5")
            })
            .await;
        with_exact.extend(names(&page));
    }
    assert_eq!(with_exact[0], "TRK-5");
    let mut unique = with_exact.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), with_exact.len());
}

#[tokio::test]
async fn limits_are_checked() {
    let t = setup().await;
    for (limit, offset) in [(Some(0), None), (Some(101), None), (None, Some(-1))] {
        let err = t
            .store
            .read(move |c| {
                search::search(
                    c,
                    &SearchQuery {
                        limit,
                        offset,
                        ..q("x")
                    },
                )
            })
            .await
            .unwrap_err();
        assert!(
            matches!(rejection(err), Rejection::Invalid(_)),
            "{limit:?} {offset:?}"
        );
    }
}

#[tokio::test]
async fn snippets_come_as_segments_that_cannot_carry_markup() {
    let t = setup().await;
    let n = t
        .issue(
            "TRK",
            "<b>bold</b> needle title",
            "before <script>alert(1)</script> \u{1}fake\u{2} needle after \u{1} dangling",
        )
        .await;
    t.comment("TRK", n, "x </mark><mark> needle \u{2}\u{1}")
        .await;
    let found = t.run(q("needle")).await;
    assert_eq!(found.hits.len(), 2);
    for hit in &found.hits {
        for segment in hit.snippet.iter().chain(&hit.title) {
            assert!(!segment.text.contains('\u{1}') && !segment.text.contains('\u{2}'));
        }
        let matched: Vec<&str> = hit
            .snippet
            .iter()
            .chain(&hit.title)
            .filter(|s| s.matched)
            .map(|s| s.text.as_str())
            .collect();
        assert!(!matched.is_empty());
        assert!(matched.iter().all(|m| *m == "needle"), "{matched:?}");
    }
    let issue = found.hits.iter().find(|h| h.kind == Kind::Issue).unwrap();
    let joined: String = issue.snippet.iter().map(|s| s.text.as_str()).collect();
    assert!(
        joined.contains("<script>alert(1)</script>"),
        "text is kept verbatim"
    );
    assert!(joined.contains("fake"), "the stray markers became spaces");
    assert!(issue.title.iter().any(|s| s.matched));
    assert!(issue.title.iter().any(|s| s.text.contains("<b>bold</b>")));
}

#[test]
fn segments_split_on_markers() {
    use crate::search::{Segment, segments};
    let s = |text: &str, matched| Segment {
        text: text.into(),
        matched,
    };
    assert_eq!(
        segments("a \u{1}b\u{2} c"),
        vec![s("a ", false), s("b", true), s(" c", false)]
    );
    assert_eq!(segments(""), vec![]);
    assert_eq!(segments("\u{1}\u{2}"), vec![]);
}

// ---- query parsing ----

#[test]
fn the_parser_quotes_everything() {
    let fts = |text: &str| parse_query(text).unwrap().fts;
    assert_eq!(fts("foo"), "\"foo\" *");
    assert_eq!(fts("foo bar"), "\"foo\" \"bar\" *");
    assert_eq!(fts("\"foo bar\""), "\"foo bar\"");
    assert_eq!(fts("\"foo bar\" baz"), "\"foo bar\" \"baz\" *");
    assert_eq!(fts("foo \"unclosed phrase"), "\"foo\" \"unclosed phrase\"");
    assert_eq!(fts("NEAR("), "\"NEAR(\" *");
    assert_eq!(fts("AND OR NOT"), "\"AND\" \"OR\" \"NOT\" *");
    assert_eq!(fts("col:value"), "\"col:value\" *");
    assert_eq!(
        fts("-word ^start (a OR b)"),
        "\"-word\" \"^start\" \"(a\" \"OR\" \"b)\" *"
    );
    assert_eq!(fts("a\0b"), "\"a\" \"b\" *");
    assert_eq!(fts("  spaced   out  "), "\"spaced\" \"out\" *");
}

#[test]
fn identifier_shapes() {
    let id = |text: &str| parse_query(text).unwrap().identifier;
    assert_eq!(id("TRK-12"), Some(("TRK".into(), 12)));
    assert_eq!(id("trk 12"), Some(("TRK".into(), 12)));
    assert_eq!(id("ab1-7"), Some(("AB1".into(), 7)));
    assert_eq!(id("trk"), None);
    assert_eq!(id("trk-x"), None);
    assert_eq!(id("1rk-2"), None);
    assert_eq!(id("trk 12 more"), None);
    assert_eq!(id("trk-1234567890"), None);
}

#[test]
fn queries_without_words_are_refused() {
    for text in [
        "",
        "   ",
        "\"",
        "\"\"",
        "***",
        "()",
        "^",
        "-",
        "\0",
        "\t\n",
        "\u{1f600}",
        " \" \" ",
    ] {
        let err = parse_query(text).unwrap_err();
        assert!(
            matches!(rejection(err), Rejection::Invalid(ref m) if m.contains("empty")),
            "{text:?}"
        );
    }
}

#[test]
fn the_size_limits_hold() {
    assert!(parse_query(&"a".repeat(MAX_QUERY_CHARS)).is_ok());
    assert!(parse_query(&"a".repeat(MAX_QUERY_CHARS + 1)).is_err());
    assert!(
        parse_query(&"\u{e9}".repeat(MAX_QUERY_CHARS)).is_ok(),
        "characters, not bytes"
    );
    let words = |n: usize| vec!["w"; n].join(" ");
    assert!(parse_query(&words(MAX_TOKENS)).is_ok());
    assert!(parse_query(&words(MAX_TOKENS + 1)).is_err());
}

#[tokio::test]
async fn nasty_input_is_a_result_or_a_refusal_never_a_database_error() {
    let t = setup().await;
    let n = t
        .issue("TRK", "near and or not", "col value (x) \"y\" * ^ -z")
        .await;
    t.comment("TRK", n, "col:value NEAR(a b) AND OR NOT").await;
    let long = "word ".repeat(40);
    let inputs = [
        "\"",
        "*",
        "**",
        "NEAR(",
        "NEAR(a b, 1)",
        "AND OR NOT",
        "AND",
        "OR",
        "NOT",
        "a AND",
        "col:value",
        "title:near",
        "body : x",
        "{title body}: near",
        "^",
        "^near",
        "(",
        ")",
        "(near",
        "near)",
        "((()))",
        "\"unbalanced",
        "unbalanced\"",
        "\"a\"b\"c\"",
        "'single",
        "a'b",
        "\\",
        "\\\"",
        "-near",
        "near -and",
        "+near",
        "near*",
        "*near",
        "ne*ar",
        "a\0b",
        "\0",
        "\u{1}\u{2}",
        "\u{1}near\u{2}",
        "near\u{7f}",
        "\u{1f600}",
        "\u{1f600} near",
        "\u{65e5}\u{672c}",
        "\u{627}\u{644}\u{639}\u{631}\u{628}\u{64a}\u{629}",
        "e\u{301}",
        "%",
        "_",
        "a%",
        "x'; DROP TABLE issue; --",
        "1",
        "12",
        "0",
        "TRK-",
        "-12",
        "TRK--1",
        "TRK-12-13",
        "trk\u{0}12",
        &long,
        "a b c d e f g h i j k l",
        "one \"two three\" four",
    ];
    for input in inputs {
        let text = input.to_string();
        let outcome = t
            .store
            .read(move |c| {
                search::search(
                    c,
                    &SearchQuery {
                        q: text,
                        include_archived: true,
                        ..SearchQuery::default()
                    },
                )
            })
            .await;
        match outcome {
            Ok(_) => {}
            Err(StoreError::Rejected(Rejection::Invalid(_))) => {}
            Err(other) => panic!("{input:?}: {other}"),
        }
    }
    // The table the last input tried to drop is still there.
    t.check().await;
    // Operators are words: this finds the issue by its text.
    assert_eq!(t.names("near AND or").await.len(), 2);
}

// ---- performance ----

fn pseudo_text(rng: &mut Rng, vocabulary: &[String], words: usize) -> String {
    // The last ten words are everyday ones and make up a third of the text.
    let common = &vocabulary[vocabulary.len() - 10..];
    (0..words)
        .map(|_| {
            if rng.below(3) == 0 {
                common[rng.below(10)].as_str()
            } else {
                vocabulary[rng.below(vocabulary.len())].as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 5,000 issues of about 40 words, 2,000 comments of 25 and 200 documents of
/// 150, then the plan and the timings of typical queries.
#[tokio::test]
async fn search_on_thousands_of_rows_is_indexed_and_fast() {
    let t = setup().await;
    let mut rng = Rng(42);
    let vocabulary: Vec<String> = (0..1500)
        .map(|i| {
            let stem = [
                "render", "queue", "login", "export", "cache", "parser", "board", "sync",
            ][i % 8];
            format!("{stem}{}", i / 8)
        })
        .chain(
            [
                "the", "and", "with", "for", "when", "fails", "after", "update", "button", "error",
            ]
            .map(String::from),
        )
        .collect();
    let mut texts: Vec<(String, String)> = (0..5000)
        .map(|_| {
            (
                pseudo_text(&mut rng, &vocabulary, 6),
                pseudo_text(&mut rng, &vocabulary, 40),
            )
        })
        .collect();
    texts[1233].0.push_str(" uniqueterm");
    let comment_texts: Vec<String> = (0..2000)
        .map(|_| pseudo_text(&mut rng, &vocabulary, 25))
        .collect();
    let document_texts: Vec<(String, String)> = (0..200)
        .map(|_| {
            (
                pseudo_text(&mut rng, &vocabulary, 4),
                pseudo_text(&mut rng, &vocabulary, 150),
            )
        })
        .collect();
    t.store
        .write(move |tx| {
            let actor = test_actor(tx);
            let (column, issue_type): (i64, i64) = tx.query_row(
                "SELECT (SELECT id FROM \"column\" WHERE project_id = 1 LIMIT 1),
                        (SELECT id FROM issue_type WHERE project_id = 1)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            for (i, (title, description)) in texts.iter().enumerate() {
                tx.execute(
                    "INSERT INTO issue (project_id, number, type_id, column_id, title, description,
                                        position, created, updated)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5, 1.0, 1, 1)",
                    rusqlite::params![i as i64 + 1, issue_type, column, title, description],
                )?;
            }
            for (i, body) in comment_texts.iter().enumerate() {
                tx.execute(
                    "INSERT INTO comment (issue_id, author_id, body, created) VALUES (?1, ?2, ?3, 1)",
                    rusqlite::params![i as i64 % 5000 + 1, actor.user_id, body],
                )?;
            }
            for (i, (title, body)) in document_texts.iter().enumerate() {
                tx.execute(
                    "INSERT INTO document (project_id, title, body, position, created, updated)
                     VALUES (1, ?1, ?2, ?3, 1, 1)",
                    rusqlite::params![title, body, i as f64],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    t.check().await;

    let queries: Vec<(&str, SearchQuery)> = vec![
        ("common word", q("error")),
        ("two common words", q("button error")),
        ("rare word", q("uniqueterm")),
        ("rare prefix", q("uniquet")),
        ("mid-frequency word", q("render77")),
        ("frequent prefix", q("render7")),
        ("short prefix", q("re")),
        ("phrase", q("\"update button\"")),
        ("identifier", q("TRK-4321")),
        (
            "filtered by project",
            SearchQuery {
                project: Some("TRK".into()),
                ..q("error")
            },
        ),
        (
            "comments only",
            SearchQuery {
                kinds: Some(vec![Kind::Comment]),
                ..q("error")
            },
        ),
        (
            "page 5",
            SearchQuery {
                offset: Some(80),
                limit: Some(20),
                ..q("error")
            },
        ),
    ];
    let mut worst = 0.0_f64;
    for (label, query) in &queries {
        let run = |query: SearchQuery| {
            let started = Instant::now();
            let found = t.store.read(move |c| search::search(c, &query));
            (started, found)
        };
        // One run warms the caches; the next ones are timed.
        let (_, first) = run(query.clone());
        let first = first.await.unwrap();
        let mut times = Vec::new();
        for _ in 0..5 {
            let (started, found) = run(query.clone());
            found.await.unwrap();
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        worst = worst.max(median);
        eprintln!(
            "search {label:<22} total {:>5} hits {:>3}  min {:>6.2} ms  median {median:>6.2} ms  max {:>6.2} ms",
            first.total,
            first.hits.len(),
            times[0],
            times[times.len() - 1]
        );
    }
    eprintln!("slowest median: {worst:.2} ms");
    assert!(worst < 100.0, "a warm search took {worst:.1} ms");

    let capped = t.run(q("error")).await;
    assert!(capped.total_capped && capped.total == search::TOTAL_CAP);
    assert_eq!(capped.hits.len(), 20);

    // The plans: the index answers the query and rows are fetched by id.
    for (label, query) in queries {
        let plan = t
            .store
            .read(move |c| search::explain(c, &query))
            .await
            .unwrap();
        eprintln!("plan {label}:\n  {}", plan.join("\n  "));
        assert!(
            plan.iter().any(|l| l.contains("VIRTUAL TABLE INDEX")),
            "{label}: the FTS index is used"
        );
        for line in &plan {
            for table in ["issue", "comment", "document", "project"] {
                let scans = line.starts_with(&format!("SCAN {table}"))
                    || line.starts_with(&format!("SCAN {table} "));
                assert!(!scans, "{label}: {table} is scanned: {line}");
            }
        }
    }
}
