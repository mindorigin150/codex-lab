use std::borrow::Cow;

use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator = sqlx::migrate!("./thread_history_migrations");

/// Allow an older Codex binary to open a database that has already been
/// migrated by a newer binary running in parallel.
///
/// We intentionally ignore applied migration versions that are newer than the
/// embedded migration set. Known migration versions are still validated by
/// checksum, so this only relaxes the "database is ahead of me" case.
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator {
        migrations: Cow::Borrowed(base.migrations.as_ref()),
        ignore_missing: true,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

pub(crate) fn runtime_state_migrator() -> Migrator {
    runtime_migrator(&STATE_MIGRATOR)
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

#[cfg(test)]
pub(crate) async fn repair_legacy_state_migration_versions(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let mut connection = pool.acquire().await?;
    repair_legacy_state_migration_versions_on_connection(&mut connection, migrator).await
}

pub(crate) async fn migrate_state_database(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    if let Some(migration) = migrator.migrations.iter().find(|migration| migration.no_tx) {
        anyhow::bail!(
            "state migration {} cannot run outside the atomic migration transaction",
            migration.version
        );
    }
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    repair_legacy_state_migration_versions_on_connection(&mut transaction, migrator).await?;
    migrator
        .run_direct(/*target*/ None, &mut *transaction, /*skip*/ false)
        .await?;
    transaction.commit().await?;
    Ok(())
}

async fn repair_legacy_state_migration_versions_on_connection(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(());
    }

    for (legacy_version, current_version) in [(38_i64, 39_i64), (41_i64, 43_i64)] {
        let Some(current_migration) = migrator
            .migrations
            .iter()
            .find(|migration| migration.version == current_version)
        else {
            continue;
        };
        let legacy_migration_needs_repair = sqlx::query_scalar::<_, i64>(
            r#"
SELECT 1
FROM _sqlx_migrations
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
            "#,
        )
        .bind(legacy_version)
        .bind(current_migration.checksum.as_ref())
        .bind(current_migration.version)
        .fetch_optional(&mut *connection)
        .await?
        .is_some();
        if !legacy_migration_needs_repair {
            continue;
        }

        sqlx::query(
            r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
            "#,
        )
        .bind(current_migration.version)
        .bind(current_migration.description.as_ref())
        .bind(legacy_version)
        .bind(current_migration.checksum.as_ref())
        .bind(current_migration.version)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
