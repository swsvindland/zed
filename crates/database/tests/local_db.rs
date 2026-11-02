//! End-to-end checks against a real SQL Server. Opt in by setting `ZED_DATABASE_TEST_SQL_SERVER`
//! to a host, e.g. `(localdb)\MSSQLLocalDB`; integrated authentication is used.

use database::{
    Authentication, Client, ConnectionProfile, DriverKind, StatementOutcome,
    sql::{self, RowChange},
};

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
        .editability(query, people)
        .await?
        .map_err(anyhow::Error::msg)?;
    assert_eq!(target.key_columns, vec![0]);
    assert_eq!(target.writable, vec![false, true, true, false]);

    let no_key = client
        .editability("SELECT Name FROM dbo.People", people)
        .await?;
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
