//! End-to-end checks against a real SQL Server. Opt in by setting `ZED_DATABASE_TEST_SQL_SERVER`
//! to a host, e.g. `(localdb)\MSSQLLocalDB`; integrated authentication is used.

use std::time::{Duration, Instant};

use database::{
    Authentication, Client, ConnectionProfile, DriverKind, ExecutionEnd, ExecutionEvent,
    ExecutionHandle, ExecutionOptions, Fetch, StatementOutcome, execution,
    sql::{self, RowChange},
};
use futures::{StreamExt as _, channel::mpsc};

/// Enough rows that SQL Server is still producing them while the first page is read.
const ENDLESS: &str = "SELECT ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n \
     FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c";

async fn rows_until_pause(events: &mut mpsc::UnboundedReceiver<ExecutionEvent>) -> usize {
    let mut rows = 0;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await
            .expect("timed out waiting for a page")
            .expect("the run ended before pausing");
        match event {
            ExecutionEvent::Rows(page) => rows += page.len(),
            ExecutionEvent::Paused => return rows,
            _ => {}
        }
    }
}

fn remaining_events(events: &mut mpsc::UnboundedReceiver<ExecutionEvent>) -> Vec<ExecutionEvent> {
    let mut remaining = Vec::new();
    while let Ok(event) = events.try_recv() {
        remaining.push(event);
    }
    remaining
}

async fn cancel_after(handle: ExecutionHandle, delay: Duration) {
    tokio::time::sleep(delay).await;
    handle.cancel();
}

/// The session must survive a stop: the temp table created at the start is still there.
async fn assert_session_kept(client: &mut Client) -> anyhow::Result<()> {
    let output = client.run("SELECT COUNT(*) FROM #session_marker").await?;
    assert!(
        matches!(&output.outcomes[..], [StatementOutcome::Rows(_)]),
        "{:?}",
        output.outcomes
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_server_cancel_paging_and_timeout() -> anyhow::Result<()> {
    let Some(profile) = profile() else {
        eprintln!("ZED_DATABASE_TEST_SQL_SERVER not set; skipping");
        return Ok(());
    };
    let mut client = Client::connect(&profile, None, None).await?;
    // Lives as long as the session does.
    client.run("CREATE TABLE #session_marker (Id int)").await?;

    // NULL and the empty string stay distinct.
    let output = client.run("SELECT NULL AS a, '' AS b").await?;
    let [StatementOutcome::Rows(result)] = &output.outcomes[..] else {
        panic!("expected rows: {:?}", output.outcomes);
    };
    assert_eq!(result.rows, vec![vec![None, Some(String::new())]]);

    // Stop a statement the server is still working on, with and without a result set to come.
    for sql in [
        "WAITFOR DELAY '00:00:30'; SELECT 1 AS One",
        "DECLARE @wait int = 1; WAITFOR DELAY '00:00:30'",
    ] {
        let (handle, mut run, _events) = execution(ExecutionOptions::default());
        let started = Instant::now();
        let (end, ()) = futures::join!(
            client.execute(sql, &mut run),
            cancel_after(handle, Duration::from_millis(500))
        );
        assert_eq!(end?, ExecutionEnd::Cancelled, "{sql}");
        assert!(started.elapsed() < Duration::from_secs(10), "{sql}");
        assert_session_kept(&mut client).await?;
    }

    // A timeout cancels the statement the same way and keeps the session.
    let (_handle, mut run, _events) = execution(ExecutionOptions {
        fetch_limit: None,
        timeout: Some(Duration::from_secs(1)),
    });
    let end = client
        .execute("WAITFOR DELAY '00:00:30'; SELECT 1 AS One", &mut run)
        .await?;
    assert_eq!(end, ExecutionEnd::TimedOut(Duration::from_secs(1)));
    assert_session_kept(&mut client).await?;

    // Paging: the first page, the next page, then everything.
    let (handle, mut run, mut events) = execution(ExecutionOptions {
        fetch_limit: Some(100),
        timeout: None,
    });
    let sql = "SELECT TOP (2500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n \
               FROM sys.all_objects a CROSS JOIN sys.all_objects b";
    let controller = async {
        assert_eq!(rows_until_pause(&mut events).await, 100);
        handle.fetch(Fetch::NextPage);
        assert_eq!(rows_until_pause(&mut events).await, 100);
        handle.fetch(Fetch::All);
    };
    let (end, ()) = futures::join!(client.execute(sql, &mut run), controller);
    assert_eq!(end?, ExecutionEnd::Completed);
    drop(run);
    let rest = remaining_events(&mut events)
        .into_iter()
        .flat_map(|event| match event {
            ExecutionEvent::Rows(rows) => rows,
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    assert_eq!(rest.len(), 2300);
    assert_eq!(rest.last(), Some(&vec![Some("2500".to_string())]));

    // Closing a paused result: a small remainder is drained, a large one is cancelled.
    for sql in [sql, ENDLESS] {
        let (handle, mut run, mut events) = execution(ExecutionOptions {
            fetch_limit: Some(100),
            timeout: None,
        });
        let controller = async {
            assert_eq!(rows_until_pause(&mut events).await, 100);
            handle.cancel();
        };
        let started = Instant::now();
        let (end, ()) = futures::join!(client.execute(sql, &mut run), controller);
        assert_eq!(end?, ExecutionEnd::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_session_kept(&mut client).await?;
    }

    // Stopping while rows stream in, after asking for everything.
    let (handle, mut run, mut events) = execution(ExecutionOptions {
        fetch_limit: Some(100),
        timeout: None,
    });
    // A task of its own, as in the app: in the same task, the busy reader would use up Tokio's
    // cooperative budget and starve the timer.
    let controller = tokio::spawn(async move {
        rows_until_pause(&mut events).await;
        handle.fetch(Fetch::All);
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel();
    });
    let started = Instant::now();
    let end = client.execute(ENDLESS, &mut run).await;
    controller.await?;
    assert_eq!(end?, ExecutionEnd::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_session_kept(&mut client).await?;

    Ok(())
}

fn profile() -> Option<ConnectionProfile> {
    let host = std::env::var("ZED_DATABASE_TEST_SQL_SERVER").ok()?;
    let mut profile = ConnectionProfile::new(DriverKind::SqlServer);
    profile.host = host;
    profile.authentication = Authentication::Integrated;
    profile.trust_server_certificate = true;
    Some(profile)
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_server_round_trip() -> anyhow::Result<()> {
    let Some(profile) = profile() else {
        eprintln!("ZED_DATABASE_TEST_SQL_SERVER not set; skipping");
        return Ok(());
    };

    let mut master = Client::connect(&profile, None, None).await?;
    let databases = master.list_databases().await?;
    assert!(databases.iter().any(|name| name == "master"));
    master
        .run("IF DB_ID('ZedDatabaseTest') IS NULL CREATE DATABASE ZedDatabaseTest")
        .await?;

    let mut client = Client::connect(&profile, Some("ZedDatabaseTest"), None).await?;
    client
        .run(
            "DROP TABLE IF EXISTS dbo.People;\n\
             CREATE TABLE dbo.People (Id int IDENTITY PRIMARY KEY, Name nvarchar(50) NOT NULL, \
               Age int NULL, Born datetime2 NULL, Total AS (Age * 2))",
        )
        .await?;
    let inserted = client
        .run("INSERT INTO dbo.People (Name, Age) VALUES (N'Ada', 36), (N'Linus', 28)")
        .await?;
    assert_eq!(inserted.outcomes, vec![StatementOutcome::RowsAffected(2)]);

    let output = client
        .run("SELECT Id, Name, Age, Total FROM dbo.People ORDER BY Id; SELECT 1 AS One WHERE 1 = 0")
        .await?;
    assert_eq!(output.outcomes.len(), 2);
    let StatementOutcome::Rows(people) = &output.outcomes[0] else {
        panic!("expected rows");
    };
    assert_eq!(people.rows.len(), 2);
    assert_eq!(people.rows[0][1].as_deref(), Some("Ada"));
    let StatementOutcome::Rows(empty) = &output.outcomes[1] else {
        panic!("expected an empty result set");
    };
    assert_eq!(empty.columns[0].name, "One");
    assert!(empty.rows.is_empty());

    let schemas = client.list_schemas().await?;
    assert!(schemas.iter().any(|schema| schema == "dbo"));
    let objects = client.list_objects("dbo").await?;
    assert!(objects.iter().any(|object| object.name == "People"));
    let columns = client.list_columns("dbo", "People").await?;
    assert!(
        columns
            .iter()
            .any(|column| column.name == "Id" && column.primary_key)
    );

    let query = "SELECT Id, Name, Age, Total FROM dbo.People ORDER BY Id";
    let target = client
        .editability(query)
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(target.columns.len(), people.columns.len());
    assert_eq!(target.key_columns, vec![0]);
    assert_eq!(target.writable, vec![false, true, true, false]);

    let no_key = client.editability("SELECT Name FROM dbo.People").await?;
    assert!(no_key.is_err());

    let ada_id = people.rows[0][0].clone();
    let linus_id = people.rows[1][0].clone();
    let statements = sql::generate_statements(
        sql::Dialect::SqlServer,
        &target.table,
        &target.columns,
        &[
            RowChange::Update {
                key: vec![(0, ada_id.clone())],
                values: vec![
                    (1, Some("Ada'; DROP TABLE dbo.People; --".into())),
                    (2, None),
                ],
            },
            RowChange::Insert {
                values: vec![(1, Some("Grace".into())), (2, Some("45".into()))],
            },
            RowChange::Delete {
                key: vec![(0, linus_id)],
            },
        ],
    )?;
    let counts = client.apply(&statements).await?;
    assert_eq!(counts, vec![1, 1, 1]);

    let after = client.run(query).await?;
    let StatementOutcome::Rows(after) = &after.outcomes[0] else {
        panic!("expected rows");
    };
    assert_eq!(after.rows.len(), 2);
    assert_eq!(
        after.rows[0][1].as_deref(),
        Some("Ada'; DROP TABLE dbo.People; --")
    );
    assert_eq!(after.rows[0][2], None);
    assert_eq!(after.rows[1][1].as_deref(), Some("Grace"));

    // A stale key must roll back the whole push.
    let stale = sql::generate_statements(
        sql::Dialect::SqlServer,
        &target.table,
        &target.columns,
        &[
            RowChange::Update {
                key: vec![(0, ada_id)],
                values: vec![(1, Some("Changed".into()))],
            },
            RowChange::Update {
                key: vec![(0, Some("999999".into()))],
                values: vec![(1, Some("Ghost".into()))],
            },
        ],
    )?;
    assert!(client.apply(&stale).await.is_err());
    let unchanged = client
        .run("SELECT Name FROM dbo.People ORDER BY Id")
        .await?;
    let StatementOutcome::Rows(unchanged) = &unchanged.outcomes[0] else {
        panic!("expected rows");
    };
    assert_eq!(
        unchanged.rows[0][0].as_deref(),
        Some("Ada'; DROP TABLE dbo.People; --")
    );
    // The session must still be usable after the rollback.
    client.run("SELECT 1").await?;

    Ok(())
}

/// The first value of a run that returns one result set.
async fn scalar(client: &mut Client, sql: &str) -> anyhow::Result<Option<String>> {
    let output = client.run(sql).await?;
    let [StatementOutcome::Rows(result)] = &output.outcomes[..] else {
        anyhow::bail!(
            "expected one result set from `{sql}`: {:?}",
            output.outcomes
        );
    };
    Ok(result
        .rows
        .first()
        .and_then(|row| row.first())
        .cloned()
        .flatten())
}

/// Each run is a batch on the console's session, so what one run sets up is still there in the
/// next, and data changes still report how many rows they affected.
#[tokio::test(flavor = "multi_thread")]
async fn sql_server_session_state_carries_over_between_runs() -> anyhow::Result<()> {
    let Some(profile) = profile() else {
        eprintln!("ZED_DATABASE_TEST_SQL_SERVER not set; skipping");
        return Ok(());
    };
    let completed = || vec![StatementOutcome::Completed];
    let affected = |count| vec![StatementOutcome::RowsAffected(count)];
    let value = |text: &str| Some(text.to_string());

    let mut client = Client::connect(&profile, Some("master"), None).await?;

    // `USE` and `SET` options stay in effect.
    assert_eq!(client.run("USE tempdb").await?.outcomes, completed());
    assert_eq!(
        scalar(&mut client, "SELECT DB_NAME()").await?,
        value("tempdb")
    );
    assert_eq!(
        client.run("SET LOCK_TIMEOUT 1234").await?.outcomes,
        completed()
    );
    assert_eq!(
        scalar(&mut client, "SELECT @@LOCK_TIMEOUT").await?,
        value("1234")
    );

    // A temp table created on its own outlives its run.
    assert_eq!(
        client
            .run("CREATE TABLE #items (Id int PRIMARY KEY, Name nvarchar(20) NOT NULL)")
            .await?
            .outcomes,
        completed()
    );
    assert_eq!(
        client
            .run("INSERT INTO #items VALUES (1, N'a'), (2, N'b'), (3, N'c')")
            .await?
            .outcomes,
        affected(3)
    );

    // A transaction begun in one run stays open until a later one rolls it back or commits.
    assert_eq!(client.run("BEGIN TRANSACTION").await?.outcomes, completed());
    assert_eq!(scalar(&mut client, "SELECT @@TRANCOUNT").await?, value("1"));
    assert_eq!(
        client
            .run("UPDATE #items SET Name = N'x' WHERE Id <= 2")
            .await?
            .outcomes,
        affected(2)
    );
    assert_eq!(client.run("ROLLBACK").await?.outcomes, completed());
    assert_eq!(
        scalar(&mut client, "SELECT Name FROM #items WHERE Id = 1").await?,
        value("a")
    );
    assert_eq!(client.run("BEGIN TRAN").await?.outcomes, completed());
    assert_eq!(
        client
            .run("DELETE FROM #items WHERE Id = 3")
            .await?
            .outcomes,
        affected(1)
    );
    assert_eq!(client.run("COMMIT").await?.outcomes, completed());
    assert_eq!(
        scalar(
            &mut client,
            "SELECT CONCAT(@@TRANCOUNT, ' open, ', COUNT(*), ' rows') FROM #items"
        )
        .await?,
        value("0 open, 2 rows")
    );

    // Counts survive a subquery, `SET NOCOUNT ON` and earlier statements that change no rows.
    assert_eq!(
        client
            .run("UPDATE #items SET Name = N'y' WHERE Id IN (SELECT Id FROM #items)")
            .await?
            .outcomes,
        affected(2)
    );
    assert_eq!(client.run("SET NOCOUNT ON").await?.outcomes, completed());
    assert_eq!(
        client
            .run("DECLARE @id int = 99;\nDELETE FROM #items WHERE Id = @id")
            .await?
            .outcomes,
        affected(0)
    );
    // Only the last statement's count can be read back, so a run with several changes has none.
    assert_eq!(
        client
            .run("INSERT INTO #items VALUES (4, N'd');\nINSERT INTO #items VALUES (5, N'e')")
            .await?
            .outcomes,
        completed()
    );
    assert!(
        client
            .run("INSERT INTO #items VALUES (1, N'again')")
            .await
            .is_err()
    );

    // Editing goes by the database the session is in now, not the one it connected to.
    client
        .run(
            "DROP TABLE IF EXISTS dbo.SessionNotes;\n\
             CREATE TABLE dbo.SessionNotes (Id int PRIMARY KEY, Body nvarchar(20) NULL)",
        )
        .await?;
    let target = client
        .editability("SELECT Id, Body FROM dbo.SessionNotes")
        .await?;
    assert!(target.is_ok(), "{target:?}");

    Ok(())
}
