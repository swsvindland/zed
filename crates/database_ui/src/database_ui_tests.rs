//! UI-level checks that drive the panel, a console, the results grid and a push against a real
//! SQL Server. Opt in with `ZED_DATABASE_TEST_SQL_SERVER` (see the `database` crate README).

use std::time::Duration;

use database::{
    Authentication, ConnectionProfile, ConnectionsFile, DriverKind, PoolKey, StatementOutcome,
};
use editor::Editor;
use futures::FutureExt as _;
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use project::{FakeFs, Project};
use tabular_data_preview::types::{AnyColumn, DataCellId, DataRow};
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::{
    DatabasePanel, DatabaseStore, RunStatement,
    console::{self, ConsoleTarget},
    push_modal::PushReviewModal,
    results_view::QueryResultsView,
};

const DATABASE: &str = "ZedDatabaseUiTest";

fn profile(id: &str, environment: &str) -> Option<ConnectionProfile> {
    let host = std::env::var("ZED_DATABASE_TEST_SQL_SERVER").ok()?;
    let mut profile = ConnectionProfile::new(DriverKind::SqlServer);
    profile.id = id.into();
    profile.name = format!("Test {environment}");
    profile.environment = environment.into();
    profile.host = host;
    profile.authentication = Authentication::Integrated;
    profile.trust_server_certificate = true;
    Some(profile)
}

async fn wait_until(
    cx: &mut VisualTestContext,
    what: &str,
    mut condition: impl FnMut(&mut VisualTestContext) -> bool,
) {
    for _ in 0..600 {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        cx.background_executor
            .timer(Duration::from_millis(50))
            .await;
        // Tokio work happens on real threads, so give it wall-clock time too.
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

async fn run_sql(
    store: &Entity<DatabaseStore>,
    key: PoolKey,
    sql: &'static str,
    cx: &mut VisualTestContext,
) -> Vec<StatementOutcome> {
    let task = store.update(cx, |store, cx| {
        store.with_client(
            key,
            move |client| async move { Ok(client.run(sql).await?.outcomes) }.boxed(),
            cx,
        )
    });
    let mut task = Some(task);
    let mut result = None;
    wait_until(cx, "query", |_| {
        if let Some(pending) = task.as_mut()
            && let Some(output) = pending.now_or_never()
        {
            result = Some(output);
            task = None;
        }
        result.is_some()
    })
    .await;
    result.unwrap().unwrap()
}

#[gpui::test]
async fn test_browse_query_edit_and_push(cx: &mut TestAppContext) {
    let (Some(dev), Some(prod)) = (
        profile("ui-test-dev", "dev"),
        profile("ui-test-prod", "prod"),
    ) else {
        eprintln!("ZED_DATABASE_TEST_SQL_SERVER not set; skipping");
        return;
    };
    cx.executor().allow_parking();
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        gpui_tokio::init(cx);
        crate::init(cx);
        app_state
    });
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let _ = app_state;

    let store = cx.update(|_, cx| DatabaseStore::global(cx));
    store.update(cx, |store, cx| {
        store.set_connections_for_test(
            ConnectionsFile {
                connections: vec![dev.clone(), prod.clone()],
            },
            cx,
        )
    });

    let master = PoolKey {
        connection_id: dev.id.clone(),
        database: "master".into(),
        session: "setup".into(),
    };
    run_sql(
        &store,
        master,
        "IF DB_ID('ZedDatabaseUiTest') IS NULL CREATE DATABASE ZedDatabaseUiTest",
        cx,
    )
    .await;
    let setup = PoolKey {
        connection_id: dev.id.clone(),
        database: DATABASE.into(),
        session: "setup".into(),
    };
    run_sql(
        &store,
        setup.clone(),
        "DROP TABLE IF EXISTS dbo.Items;\n\
         CREATE TABLE dbo.Items (Id int IDENTITY PRIMARY KEY, Name nvarchar(50) NULL, Qty int NULL);\n\
         INSERT INTO dbo.Items (Name, Qty) VALUES (N'Apple', 1), (N'Pear', 2), (N'Plum', 3)",
        cx,
    )
    .await;

    // Phase 1: the panel lists environments and, once expanded, the server's databases.
    let panel = workspace
        .update_in(cx, |workspace, window, cx| {
            let weak = workspace.weak_handle();
            let async_cx = window.to_async(cx);
            DatabasePanel::load(weak, async_cx)
        })
        .await
        .unwrap();
    panel.update(cx, |panel, cx| {
        panel.expand_connection_for_test(&dev.id, cx)
    });
    wait_until(cx, "databases in the panel", |cx| {
        panel.read_with(cx, |panel, _| {
            panel.visible_labels().iter().any(|label| label == DATABASE)
        })
    })
    .await;
    panel.read_with(cx, |panel, _| {
        let labels = panel.visible_labels();
        assert!(labels.iter().any(|label| label == "dev"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "prod"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "master"), "{labels:?}");
    });

    // Phase 2: run the statement under the cursor from a SQL editor bound to the database.
    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(
                "SELECT 1 AS One;\n\nSELECT Id, Name, Qty FROM dbo.Items ORDER BY Id",
                None,
                false,
                cx,
            )
        });
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    cx.update(|_, cx| {
        console::bind_editor(
            &editor,
            ConsoleTarget {
                connection_id: dev.id.clone(),
                database: DATABASE.into(),
            },
            cx,
        )
    });
    editor.update_in(cx, |editor, window, cx| {
        editor.move_to_end(&editor::actions::MoveToEnd, window, cx)
    });
    cx.dispatch_action(RunStatement);
    let results = wait_for_results(&workspace, cx).await;
    results.read_with(cx, |results, _| {
        let (rows, editable) = results.summary_for_test();
        assert_eq!(rows, vec![3]);
        assert!(editable.is_ok(), "{editable:?}");
    });

    // Phase 3: edit by typing into a cell, then review and push.
    let table = results.read_with(cx, |results, _| results.table_for_test(0));
    table.update_in(cx, |table, window, cx| {
        table.select_cell(DataCellId::new(DataRow(1), AnyColumn(1)), cx);
        window.focus(&table.focus_handle(cx), cx);
    });
    cx.simulate_keystrokes("B");
    cx.simulate_input("anana");
    cx.dispatch_action(menu::Confirm);
    results.update_in(cx, |results, window, cx| {
        results.set_null_for_test(DataCellId::new(DataRow(2), AnyColumn(2)), window, cx);
        results.review_push_for_test(window, cx);
    });
    let modal = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<PushReviewModal>(cx)
        })
        .expect("push review modal");
    modal.read_with(cx, |modal, _| {
        let preview = modal.preview_for_test();
        assert!(
            preview.contains("UPDATE [dbo].[Items] SET [Name] = @P1 WHERE [Id] = @P2"),
            "{preview}"
        );
        assert!(preview.contains("'Banana'"), "{preview}");
        assert!(preview.contains("SET [Qty] = @P1"), "{preview}");
    });
    modal.update_in(cx, |modal, window, cx| modal.push_for_test(window, cx));
    wait_until(cx, "push and refresh", |cx| {
        results.read_with(cx, |results, _| {
            results.pending_row_count() == 0
                && !results.is_busy_for_test()
                && results.value_for_test(0, 1, 1) == Some(Some("Banana".into()))
        })
    })
    .await;
    let outcomes = run_sql(
        &store,
        setup.clone(),
        "SELECT Name, Qty FROM dbo.Items ORDER BY Id",
        cx,
    )
    .await;
    let StatementOutcome::Rows(rows) = &outcomes[0] else {
        panic!("expected rows");
    };
    assert_eq!(rows.rows[1][0].as_deref(), Some("Banana"));
    assert_eq!(rows.rows[2][1], None);

    // Read-only results explain why.
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("SELECT Name FROM dbo.Items", window, cx)
    });
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    cx.dispatch_action(RunStatement);
    wait_until(cx, "read-only result", |cx| {
        results.read_with(cx, |results, _| {
            results
                .summary_for_test()
                .1
                .is_err_and(|reason| reason.contains("Primary key column Id"))
        })
    })
    .await;

    // Production connections need an acknowledgement before pushing.
    cx.update(|_, cx| {
        console::bind_editor(
            &editor,
            ConsoleTarget {
                connection_id: prod.id.clone(),
                database: DATABASE.into(),
            },
            cx,
        )
    });
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("SELECT Id, Name FROM dbo.Items ORDER BY Id", window, cx)
    });
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    cx.dispatch_action(RunStatement);
    wait_until(cx, "prod result", |cx| {
        results.read_with(cx, |results, cx| {
            let (rows, editable) = results.summary_for_test();
            rows == vec![3] && editable.is_ok() && results.is_production_for_test(cx)
        })
    })
    .await;
    results.update_in(cx, |results, window, cx| {
        results.set_null_for_test(DataCellId::new(DataRow(0), AnyColumn(1)), window, cx);
        results.review_push_for_test(window, cx);
    });
    let modal = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<PushReviewModal>(cx)
        })
        .expect("push review modal");
    modal.read_with(cx, |modal, _| assert!(!modal.can_push_for_test()));
}

async fn wait_for_results(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<QueryResultsView> {
    let mut found = None;
    wait_until(cx, "results", |cx| {
        found = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .find_map(|pane| pane.read(cx).items_of_type::<QueryResultsView>().next())
        });
        found.as_ref().is_some_and(|view| {
            let messages = view.read_with(cx, |view, _| view.messages_for_test());
            assert!(
                !messages.iter().any(|message| message.starts_with("Error")),
                "{messages:?}"
            );
            !view.read_with(cx, |view, _| view.summary_for_test().0.is_empty())
        })
    })
    .await;
    found.unwrap()
}
