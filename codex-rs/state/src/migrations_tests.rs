use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::Connection;
use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

use super::STATE_MIGRATOR;
use super::migrate_state_database;
use super::repair_legacy_state_migration_versions;
use crate::state_db_path;

fn migrator_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: STATE_MIGRATOR.ignore_missing,
        locking: STATE_MIGRATOR.locking,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        no_tx: STATE_MIGRATOR.no_tx,
    }
}

#[tokio::test]
async fn agent_job_tables_are_dropped_when_upgrading() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&state_db_path(&sqlite_home))
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 15)
        .run(&pool)
        .await
        .expect("agent job migrations should apply");

    sqlx::query(
        r#"
INSERT INTO agent_jobs (
    id,
    name,
    status,
    instruction,
    input_headers_json,
    input_csv_path,
    output_csv_path,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("legacy job")
    .bind("running")
    .bind("process rows")
    .bind(r#"["path"]"#)
    .bind("/tmp/input.csv")
    .bind("/tmp/output.csv")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job should insert");
    sqlx::query(
        r#"
INSERT INTO agent_job_items (
    job_id,
    item_id,
    row_index,
    row_json,
    status,
    result_json,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("item-1")
    .bind(0_i64)
    .bind(r#"{"path":"secret.csv"}"#)
    .bind("completed")
    .bind(r#"{"result":"legacy"}"#)
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job item should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");

    let agent_job_tables = sqlx::query_scalar::<_, String>(
        r#"
SELECT name
FROM sqlite_master
WHERE type = 'table' AND name IN ('agent_jobs', 'agent_job_items')
ORDER BY name
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("remaining agent job tables should load");
    assert_eq!(agent_job_tables, Vec::<String>::new());

    pool.close().await;
}

#[tokio::test]
async fn recency_migration_backfills_and_seeds_old_binary_inserts() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&state_db_path(&sqlite_home))
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .bind("/tmp/first.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_100_i64)
    .bind(1_700_000_000_123_i64)
    .bind(1_700_000_100_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("recency migration should apply");

    let backfilled = sqlx::query(
        "SELECT updated_at, updated_at_ms, recency_at, recency_at_ms FROM threads WHERE id = ?",
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .fetch_one(&pool)
    .await
    .expect("backfilled row should load");
    assert_eq!(backfilled.get::<i64, _>("recency_at"), 1_700_000_100);
    assert_eq!(backfilled.get::<i64, _>("recency_at_ms"), 1_700_000_100_456);

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000002")
    .bind("/tmp/second.jsonl")
    .bind(1_700_000_200_i64)
    .bind(1_700_000_300_i64)
    .bind(1_700_000_200_123_i64)
    .bind(1_700_000_300_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("old-binary row should insert");

    let seeded = sqlx::query("SELECT recency_at, recency_at_ms FROM threads WHERE id = ?")
        .bind("00000000-0000-0000-0000-000000000002")
        .fetch_one(&pool)
        .await
        .expect("old-binary row should load");
    assert_eq!(seeded.get::<i64, _>("recency_at"), 1_700_000_300);
    assert_eq!(seeded.get::<i64, _>("recency_at_ms"), 1_700_000_300_456);

    pool.close().await;
}

#[tokio::test]
async fn collaboration_mode_migration_preserves_legacy_threads() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(/*version*/ 40)
        .run(&pool)
        .await
        .expect("pre-collaboration-mode migrations should apply");

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000003")
    .bind("/tmp/legacy.jsonl")
    .bind(1_700_000_400_i64)
    .bind(1_700_000_500_i64)
    .bind(1_700_000_400_123_i64)
    .bind(1_700_000_500_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("collaboration mode migration should apply");

    let migrated = sqlx::query("SELECT collaboration_mode FROM threads WHERE id = ?")
        .bind("00000000-0000-0000-0000-000000000003")
        .fetch_one(&pool)
        .await
        .expect("migrated legacy row should load");
    assert_eq!(
        migrated.get::<Option<String>, _>("collaboration_mode"),
        None
    );
}

#[tokio::test]
async fn repairs_collaboration_mode_migration_that_was_applied_as_version_41() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = state_db_path(&sqlite_home);
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");

    let collaboration_mode_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 43)
        .expect("collaboration mode migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 40)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        41,
        collaboration_mode_migration.description.clone(),
        collaboration_mode_migration.migration_type,
        collaboration_mode_migration.sql.clone(),
        collaboration_mode_migration.no_tx,
    ));
    Migrator::with_migrations(legacy_migrations)
        .run(&pool)
        .await
        .expect("legacy collaboration mode migration should apply as version 41");
    pool.close().await;

    let runtime = crate::StateRuntime::init(sqlite_home.clone(), "test-provider".to_string())
        .await
        .expect("state runtime should repair and migrate the legacy database");
    let read_pool = sqlite
        .open_read_only_pool(&state_path)
        .await
        .expect("migrated database should open for reading");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 41 ORDER BY version",
    )
    .fetch_all(&read_pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 41)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    let mut thread_columns = sqlx::query("PRAGMA table_info(threads)")
        .fetch_all(&read_pool)
        .await
        .expect("thread columns should load")
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .filter(|name| matches!(name.as_str(), "collaboration_mode" | "name"))
        .collect::<Vec<_>>();
    thread_columns.sort();
    assert_eq!(
        thread_columns,
        vec!["collaboration_mode".to_string(), "name".to_string()]
    );

    let legacy_agent_job_tables = sqlx::query_scalar::<_, i64>(
        r#"
SELECT COUNT(*)
FROM sqlite_master
WHERE type = 'table' AND name IN ('agent_jobs', 'agent_job_items')
        "#,
    )
    .fetch_one(&read_pool)
    .await
    .expect("legacy agent job tables should be inspected");
    assert_eq!(legacy_agent_job_tables, 0);

    read_pool.close().await;
    drop(runtime);
}

#[tokio::test]
async fn repairs_recency_migration_that_was_applied_as_version_38() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&state_db_path(&sqlite_home))
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    let recency_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
        .expect("recency migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 37)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        38,
        recency_migration.description.clone(),
        recency_migration.migration_type,
        recency_migration.sql.clone(),
        recency_migration.no_tx,
    ));
    let legacy_recency_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_recency_migrator
        .run(&pool)
        .await
        .expect("legacy recency migration should apply as version 38");

    repair_legacy_state_migration_versions(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 38 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 38)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}

#[tokio::test]
async fn repair_recency_migration_succeeds_while_another_connection_holds_writer_slot() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = state_db_path(&sqlite_home);
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("database should open");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");
    let read_pool = sqlite
        .open_read_only_pool(&state_path)
        .await
        .expect("read-only pool should open");
    let mut write_connection = pool.acquire().await.expect("write connection should open");
    let write_transaction = write_connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("write transaction should acquire the writer slot");

    let repair_result = repair_legacy_state_migration_versions(&read_pool, &STATE_MIGRATOR).await;

    write_transaction
        .rollback()
        .await
        .expect("write transaction should roll back");
    drop(write_connection);
    read_pool.close().await;
    pool.close().await;
    repair_result.expect("current migration history should not need the writer slot");
}

#[tokio::test]
async fn state_migration_waits_for_legacy_writer_then_repairs_its_version() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&state_db_path(&sqlite_home))
        .await
        .expect("database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    let recency_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
        .expect("recency migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 37)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        38,
        recency_migration.description.clone(),
        recency_migration.migration_type,
        recency_migration.sql.clone(),
        recency_migration.no_tx,
    ));
    let legacy_migrator = Migrator::with_migrations(legacy_migrations);

    let mut legacy_transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("legacy migration should acquire the writer slot");
    let barrier = Arc::new(Barrier::new(2));
    let migration_pool = pool.clone();
    let migration_barrier = Arc::clone(&barrier);
    let mut current_migration = tokio::spawn(async move {
        migration_barrier.wait().await;
        migrate_state_database(&migration_pool, &STATE_MIGRATOR).await
    });
    barrier.wait().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut current_migration)
            .await
            .is_err(),
        "current migration should wait for the legacy writer"
    );

    legacy_migrator
        .run_direct(
            /*target*/ None,
            &mut *legacy_transaction,
            /*skip*/ false,
        )
        .await
        .expect("legacy recency migration should apply as version 38");
    legacy_transaction
        .commit()
        .await
        .expect("legacy migration should commit");
    current_migration
        .await
        .expect("current migration task should join")
        .expect("current migration should repair the legacy version");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 38 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 38)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}
