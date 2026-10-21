use super::*;
use fs::FakeFs;
use gpui::{BorrowAppContext as _, TestAppContext};
use language::rust_lang;
use serde_json::json;
use settings::SettingsStore;
use std::path::Path;
use util::path;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        init(cx);
    });
}

async fn candidate_names(
    project: &Entity<Project>,
    query: &str,
    cx: &mut TestAppContext,
) -> Vec<String> {
    let (symbols, _) = cx.update(|cx| candidates(project, query, 20, cx)).await;
    symbols
        .into_iter()
        .map(|symbol| symbol.name.to_string())
        .collect()
}

#[gpui::test]
async fn test_indexes_project_symbols(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "src": {} })).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(rust_lang()));
    cx.run_until_parked();

    fs.insert_file(
        path!("/root/src/geometry.rs"),
        b"struct Point { x: f32 }\nfn distance_between(a: Point, b: Point) -> f32 { 0.0 }\n"
            .to_vec(),
    )
    .await;
    fs.insert_file(path!("/root/src/notes.txt"), b"fn not_rust() {}".to_vec())
        .await;
    cx.run_until_parked();

    assert_eq!(
        candidate_names(&project, "distbet", cx).await,
        vec!["distance_between"]
    );
    assert_eq!(candidate_names(&project, "Point", cx).await, vec!["Point"]);
    assert!(candidate_names(&project, "not_rust", cx).await.is_empty());

    fs.save(
        Path::new(path!("/root/src/geometry.rs")),
        &"fn manhattan_distance() {}".into(),
        Default::default(),
    )
    .await
    .unwrap();
    cx.run_until_parked();
    assert!(candidate_names(&project, "distbet", cx).await.is_empty());
    assert_eq!(
        candidate_names(&project, "manhattan", cx).await,
        vec!["manhattan_distance"]
    );

    fs.remove_file(
        Path::new(path!("/root/src/geometry.rs")),
        Default::default(),
    )
    .await
    .unwrap();
    cx.run_until_parked();
    assert!(candidate_names(&project, "manhattan", cx).await.is_empty());
}

#[gpui::test]
async fn test_indexing_setting_pauses_index(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.indexing = Some(false);
            });
        });
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "src": {} })).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(rust_lang()));
    cx.run_until_parked();

    fs.insert_file(path!("/root/src/lib.rs"), b"fn paused_symbol() {}".to_vec())
        .await;
    cx.run_until_parked();
    assert!(candidate_names(&project, "paused", cx).await.is_empty());

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.indexing = Some(true);
            });
        });
    });
    cx.run_until_parked();
    assert_eq!(
        candidate_names(&project, "paused", cx).await,
        vec!["paused_symbol"]
    );
}

#[gpui::test]
async fn test_indexes_todo_comments(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "src": {} })).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(rust_lang()));
    cx.run_until_parked();

    fs.insert_file(
        path!("/root/src/lib.rs"),
        b"// TODO: split this up\nfn todo_free() {}\n".to_vec(),
    )
    .await;
    cx.run_until_parked();

    let todo_texts = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            project_todos(&project, cx)
                .into_iter()
                .flat_map(|file| {
                    file.todos
                        .iter()
                        .map(|todo| {
                            (
                                file.path.as_unix_str().to_string(),
                                todo.row,
                                todo.text.to_string(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        todo_texts(cx),
        vec![(
            "src/lib.rs".to_string(),
            0,
            "TODO: split this up".to_string()
        )]
    );

    fs.save(
        Path::new(path!("/root/src/lib.rs")),
        &"fn done() {}".into(),
        Default::default(),
    )
    .await
    .unwrap();
    cx.run_until_parked();
    assert!(todo_texts(cx).is_empty());
}
