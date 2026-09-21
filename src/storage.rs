use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{Provider, QuotaSnapshot, QuotaSnapshotInput, ValidationError, WindowKind};

const SCHEMA_VERSION: i64 = 3;
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_HISTORY_LIMIT: usize = 500;

/// A cloneable database handle. Each operation uses its own short-lived connection.
#[derive(Debug, Clone)]
pub struct Storage {
    database_path: Arc<PathBuf>,
    busy_timeout: Duration,
}

impl Storage {
    pub fn open(database_path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with_busy_timeout(database_path, DEFAULT_BUSY_TIMEOUT)
    }

    pub fn open_with_busy_timeout(
        database_path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, StorageError> {
        let database_path = database_path.as_ref().to_path_buf();
        if let Some(parent) = database_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let storage = Self {
            database_path: Arc::new(database_path),
            busy_timeout,
        };
        let mut connection = storage.connect()?;
        migrate(&mut connection)?;
        Ok(storage)
    }

    pub fn database_path(&self) -> &Path {
        self.database_path.as_ref()
    }

    pub fn insert_snapshots(&self, snapshots: &[QuotaSnapshot]) -> Result<usize, StorageError> {
        if snapshots.is_empty() {
            return Ok(0);
        }

        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let mut affected = 0;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO snapshots (
                    provider, scope_key, window_kind, limit_kind, window_duration_seconds,
                    used_percent, resets_at, observed_at, availability,
                    source_version, quality, source_sequence
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(provider, scope_key, window_kind, observed_at) DO UPDATE SET
                    limit_kind = excluded.limit_kind,
                    window_duration_seconds = excluded.window_duration_seconds,
                    used_percent = excluded.used_percent,
                    resets_at = excluded.resets_at,
                    availability = excluded.availability,
                    source_version = excluded.source_version,
                    quality = excluded.quality,
                    source_sequence = excluded.source_sequence",
            )?;
            for snapshot in snapshots {
                let row = snapshot.as_input();
                affected += statement.execute(params![
                    row.provider.as_str(),
                    row.scope_key,
                    row.window_kind.as_str(),
                    row.limit_kind.as_str(),
                    row.window_duration_seconds,
                    row.used_percent,
                    row.resets_at,
                    row.observed_at,
                    row.availability.as_str(),
                    row.source_version,
                    row.quality.as_str(),
                    row.source_sequence.to_string(),
                ])?;
            }
        }
        transaction.commit()?;
        Ok(affected)
    }

    pub fn latest_snapshot(
        &self,
        provider: Provider,
        scope_key: &str,
        window_kind: WindowKind,
    ) -> Result<Option<StoredSnapshot>, StorageError> {
        let connection = self.connect()?;
        let raw = connection
            .query_row(
                "SELECT id, provider, scope_key, window_kind, limit_kind, window_duration_seconds,
                        used_percent, resets_at, observed_at, availability,
                        source_version, quality, source_sequence
                 FROM snapshots
                 WHERE provider = ?1 AND scope_key = ?2 AND window_kind = ?3
                 ORDER BY observed_at DESC, id DESC
                 LIMIT 1",
                params![provider.as_str(), scope_key, window_kind.as_str()],
                RawSnapshot::from_row,
            )
            .optional()?;
        raw.map(RawSnapshot::validate).transpose()
    }

    pub fn latest_snapshots(&self) -> Result<Vec<StoredSnapshot>, StorageError> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT s.id, s.provider, s.scope_key, s.window_kind, s.limit_kind,
                    s.window_duration_seconds, s.used_percent, s.resets_at,
                    s.observed_at, s.availability, s.source_version,
                    s.quality, s.source_sequence
             FROM snapshots s
             WHERE NOT EXISTS (
                 SELECT 1 FROM snapshots newer
                 WHERE newer.provider = s.provider
                   AND newer.scope_key = s.scope_key
                   AND newer.window_kind = s.window_kind
                   AND (newer.observed_at > s.observed_at OR
                        (newer.observed_at = s.observed_at AND newer.id > s.id))
             )
             ORDER BY s.provider, s.scope_key, s.window_kind",
        )?;
        let raws = statement
            .query_map([], RawSnapshot::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(RawSnapshot::validate).collect()
    }

    pub fn previous_snapshot_before(
        &self,
        provider: Provider,
        scope_key: &str,
        window_kind: WindowKind,
        observed_at: i64,
    ) -> Result<Option<StoredSnapshot>, StorageError> {
        let connection = self.connect()?;
        let raw = connection
            .query_row(
                "SELECT id, provider, scope_key, window_kind, limit_kind, window_duration_seconds,
                        used_percent, resets_at, observed_at, availability,
                        source_version, quality, source_sequence
                 FROM snapshots
                 WHERE provider = ?1 AND scope_key = ?2 AND window_kind = ?3
                   AND observed_at < ?4
                 ORDER BY observed_at DESC, id DESC LIMIT 1",
                params![
                    provider.as_str(),
                    scope_key,
                    window_kind.as_str(),
                    observed_at
                ],
                RawSnapshot::from_row,
            )
            .optional()?;
        raw.map(RawSnapshot::validate).transpose()
    }

    pub fn recent_window_history(
        &self,
        provider: Provider,
        scope_key: &str,
        window_kind: WindowKind,
        limit: usize,
    ) -> Result<Vec<StoredSnapshot>, StorageError> {
        self.query_history(
            "WHERE provider = ?1 AND scope_key = ?2 AND window_kind = ?3",
            params![provider.as_str(), scope_key, window_kind.as_str()],
            limit,
        )
    }

    pub fn recent_history(&self, limit: usize) -> Result<Vec<StoredSnapshot>, StorageError> {
        self.query_history("", [], limit)
    }

    fn query_history<P: rusqlite::Params>(
        &self,
        filter: &str,
        params: P,
        limit: usize,
    ) -> Result<Vec<StoredSnapshot>, StorageError> {
        validate_history_limit(limit)?;
        let connection = self.connect()?;
        let sql = format!(
            "SELECT id, provider, scope_key, window_kind, limit_kind, window_duration_seconds,
                    used_percent, resets_at, observed_at, availability,
                    source_version, quality, source_sequence
             FROM snapshots {filter}
             ORDER BY observed_at DESC, id DESC LIMIT {limit}"
        );
        let mut statement = connection.prepare(&sql)?;
        let raws = statement
            .query_map(params, RawSnapshot::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(RawSnapshot::validate).collect()
    }

    /// Atomically records an alert identity, returning true only to its first claimant.
    pub fn claim_alert(
        &self,
        provider: Provider,
        scope_key: &str,
        window_kind: WindowKind,
        alert_kind: &str,
        cycle_key: &str,
        fired_at: i64,
    ) -> Result<bool, StorageError> {
        let connection = self.connect()?;
        let changed = connection.execute(
            "INSERT OR IGNORE INTO alert_state (
                provider, scope_key, window_kind, alert_kind, cycle_key, fired_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                provider.as_str(),
                scope_key,
                window_kind.as_str(),
                alert_kind,
                cycle_key,
                fired_at
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn recent_alerts(&self, limit: usize) -> Result<Vec<AlertRecord>, StorageError> {
        validate_history_limit(limit)?;
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT provider, scope_key, window_kind, alert_kind, cycle_key, fired_at
             FROM alert_state ORDER BY fired_at DESC LIMIT ?1",
        )?;
        statement
            .query_map([limit as i64], |row| {
                Ok(RawAlert {
                    provider: row.get(0)?,
                    scope_key: row.get(1)?,
                    window_kind: row.get(2)?,
                    alert_kind: row.get(3)?,
                    cycle_key: row.get(4)?,
                    fired_at: row.get(5)?,
                })
            })?
            .map(|raw| raw.map_err(StorageError::from).and_then(RawAlert::validate))
            .collect()
    }

    pub fn record_provider_attempt(
        &self,
        provider: Provider,
        attempted_at: i64,
    ) -> Result<(), StorageError> {
        let connection = self.connect()?;
        connection.execute(
            "INSERT INTO provider_state (provider, last_attempt_at)
             VALUES (?1, ?2)
             ON CONFLICT(provider) DO UPDATE SET last_attempt_at = excluded.last_attempt_at",
            params![provider.as_str(), attempted_at],
        )?;
        Ok(())
    }

    pub fn record_provider_success(
        &self,
        provider: Provider,
        observed_at: i64,
    ) -> Result<(), StorageError> {
        let connection = self.connect()?;
        connection.execute(
            "INSERT INTO provider_state (
                provider, last_success_at, last_attempt_at, consecutive_failures, last_error_class
             ) VALUES (?1, ?2, ?2, 0, NULL)
             ON CONFLICT(provider) DO UPDATE SET
                last_success_at = excluded.last_success_at,
                last_attempt_at = excluded.last_attempt_at,
                consecutive_failures = 0,
                last_error_class = NULL",
            params![provider.as_str(), observed_at],
        )?;
        Ok(())
    }

    pub fn record_provider_failure(
        &self,
        provider: Provider,
        attempted_at: i64,
        error_class: &'static str,
    ) -> Result<(), StorageError> {
        let connection = self.connect()?;
        connection.execute(
            "INSERT INTO provider_state (
                provider, last_attempt_at, consecutive_failures, last_error_class
             ) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                last_attempt_at = excluded.last_attempt_at,
                consecutive_failures = provider_state.consecutive_failures + 1,
                last_error_class = excluded.last_error_class",
            params![provider.as_str(), attempted_at, error_class],
        )?;
        Ok(())
    }

    pub fn provider_state(
        &self,
        provider: Provider,
    ) -> Result<Option<ProviderState>, StorageError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT last_success_at, last_attempt_at, consecutive_failures, last_error_class
                 FROM provider_state WHERE provider = ?1",
                [provider.as_str()],
                |row| {
                    Ok(ProviderState {
                        provider,
                        last_success_at: row.get(0)?,
                        last_attempt_at: row.get(1)?,
                        consecutive_failures: row.get(2)?,
                        last_error_class: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// Deletes observations older than the caller-computed retention cutoff.
    pub fn prune_before(&self, observed_at_cutoff: i64) -> Result<usize, StorageError> {
        if observed_at_cutoff < 0 {
            return Err(StorageError::InvalidRetentionCutoff(observed_at_cutoff));
        }
        let connection = self.connect()?;
        Ok(connection.execute(
            "DELETE FROM snapshots WHERE observed_at < ?1",
            [observed_at_cutoff],
        )?)
    }

    pub fn prune_alerts_before(&self, fired_at_cutoff: i64) -> Result<usize, StorageError> {
        if fired_at_cutoff < 0 {
            return Err(StorageError::InvalidRetentionCutoff(fired_at_cutoff));
        }
        let connection = self.connect()?;
        Ok(connection.execute(
            "DELETE FROM alert_state WHERE fired_at < ?1",
            [fired_at_cutoff],
        )?)
    }

    fn connect(&self) -> Result<Connection, StorageError> {
        let connection = Connection::open(self.database_path())?;
        connection.busy_timeout(self.busy_timeout)?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(connection)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredSnapshot {
    pub id: i64,
    pub snapshot: QuotaSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRecord {
    pub provider: Provider,
    pub scope_key: String,
    pub window_kind: WindowKind,
    pub alert_kind: String,
    pub cycle_key: String,
    pub fired_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderState {
    pub provider: Provider,
    pub last_success_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub consecutive_failures: i64,
    pub last_error_class: Option<String>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("stored {field} value is invalid")]
    InvalidStoredValue { field: &'static str },
    #[error("stored snapshot violates the normalized domain: {0}")]
    InvalidStoredSnapshot(#[from] ValidationError),
    #[error("database migration did not preserve every snapshot")]
    MigrationInvariant,
    #[error("retention cutoff must be a nonnegative Unix millisecond, got {0}")]
    InvalidRetentionCutoff(i64),
    #[error("history limit must be between 1 and {MAX_HISTORY_LIMIT}, got {0}")]
    InvalidHistoryLimit(usize),
}

fn validate_history_limit(limit: usize) -> Result<(), StorageError> {
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(StorageError::InvalidHistoryLimit(limit));
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    let current: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchemaVersion {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    if current == 0 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(CREATE_SNAPSHOTS_V3)?;
        transaction.execute_batch(CREATE_SUPPORTING_TABLES)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
    } else if current == 1 {
        migrate_v1_to_v3(connection)?;
    } else if current == 2 {
        migrate_v2_to_v3(connection)?;
    }
    Ok(())
}

fn migrate_v1_to_v3(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection.transaction()?;
    let before: i64 =
        transaction.query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))?;
    transaction.execute_batch(
        "DROP INDEX snapshots_latest;
         ALTER TABLE snapshots RENAME TO snapshots_v1;",
    )?;
    transaction.execute_batch(CREATE_SNAPSHOTS_V3)?;
    transaction.execute(
        "INSERT INTO snapshots (
            id, provider, scope_key, window_kind, limit_kind,
            window_duration_seconds, used_percent, resets_at, observed_at,
            availability, source_version, quality, source_sequence
         )
         SELECT id, provider, scope_key, window_kind, 'limited',
                window_duration_seconds, used_percent, resets_at, observed_at,
                availability, source_version, quality, source_sequence
         FROM snapshots_v1",
        [],
    )?;
    verify_snapshot_count(&transaction, before)?;
    transaction.execute("DROP TABLE snapshots_v1", [])?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v2_to_v3(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection.transaction()?;
    let before: i64 =
        transaction.query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))?;
    transaction.execute_batch(
        "DROP INDEX snapshots_latest;
         ALTER TABLE snapshots RENAME TO snapshots_v2;",
    )?;
    transaction.execute_batch(CREATE_SNAPSHOTS_V3)?;
    transaction.execute(
        "INSERT INTO snapshots (
            id, provider, scope_key, window_kind, limit_kind,
            window_duration_seconds, used_percent, resets_at, observed_at,
            availability, source_version, quality, source_sequence
         )
         SELECT id, provider, scope_key, window_kind, limit_kind,
                window_duration_seconds, used_percent, resets_at, observed_at,
                availability, source_version, quality, source_sequence
         FROM snapshots_v2",
        [],
    )?;
    verify_snapshot_count(&transaction, before)?;
    transaction.execute("DROP TABLE snapshots_v2", [])?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn verify_snapshot_count(
    transaction: &rusqlite::Transaction<'_>,
    expected: i64,
) -> Result<(), StorageError> {
    let actual: i64 =
        transaction.query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))?;
    if actual != expected {
        return Err(StorageError::MigrationInvariant);
    }
    Ok(())
}

const CREATE_SNAPSHOTS_V3: &str =
    "CREATE TABLE snapshots (
        id INTEGER PRIMARY KEY,
        provider TEXT NOT NULL CHECK (provider IN ('codex','claude','github_copilot','google_antigravity')),
        scope_key TEXT NOT NULL,
        window_kind TEXT NOT NULL CHECK (window_kind IN ('rolling_5h','rolling_7d','spend','monthly','other')),
        limit_kind TEXT NOT NULL CHECK (limit_kind IN ('limited','unlimited')),
        window_duration_seconds INTEGER,
        used_percent REAL NOT NULL,
        resets_at INTEGER,
        observed_at INTEGER NOT NULL,
        availability TEXT NOT NULL CHECK (availability IN ('allowed','blocked','unknown')),
        source_version TEXT,
        quality TEXT NOT NULL CHECK (quality IN ('fresh','partial')),
        source_sequence TEXT NOT NULL,
        CHECK (used_percent >= 0),
        UNIQUE(provider, scope_key, window_kind, observed_at)
     );
     CREATE INDEX snapshots_latest
        ON snapshots(provider, scope_key, window_kind, observed_at DESC);";

const CREATE_SUPPORTING_TABLES: &str = "CREATE TABLE provider_state (
        provider TEXT PRIMARY KEY,
        last_success_at INTEGER,
        last_attempt_at INTEGER,
        consecutive_failures INTEGER NOT NULL DEFAULT 0,
        last_error_class TEXT
     );
     CREATE TABLE alert_state (
        provider TEXT NOT NULL,
        scope_key TEXT NOT NULL,
        window_kind TEXT NOT NULL,
        alert_kind TEXT NOT NULL,
        cycle_key TEXT NOT NULL,
        fired_at INTEGER NOT NULL,
        PRIMARY KEY(provider, scope_key, window_kind, alert_kind, cycle_key)
     );";

struct RawSnapshot {
    id: i64,
    provider: String,
    scope_key: String,
    window_kind: String,
    limit_kind: String,
    window_duration_seconds: Option<i64>,
    used_percent: f64,
    resets_at: Option<i64>,
    observed_at: i64,
    availability: String,
    source_version: Option<String>,
    quality: String,
    source_sequence: String,
}

struct RawAlert {
    provider: String,
    scope_key: String,
    window_kind: String,
    alert_kind: String,
    cycle_key: String,
    fired_at: i64,
}

impl RawAlert {
    fn validate(self) -> Result<AlertRecord, StorageError> {
        Ok(AlertRecord {
            provider: parse_stored("provider", &self.provider)?,
            scope_key: self.scope_key,
            window_kind: parse_stored("window_kind", &self.window_kind)?,
            alert_kind: self.alert_kind,
            cycle_key: self.cycle_key,
            fired_at: self.fired_at,
        })
    }
}

impl RawSnapshot {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            provider: row.get(1)?,
            scope_key: row.get(2)?,
            window_kind: row.get(3)?,
            limit_kind: row.get(4)?,
            window_duration_seconds: row.get(5)?,
            used_percent: row.get(6)?,
            resets_at: row.get(7)?,
            observed_at: row.get(8)?,
            availability: row.get(9)?,
            source_version: row.get(10)?,
            quality: row.get(11)?,
            source_sequence: row.get(12)?,
        })
    }

    fn validate(self) -> Result<StoredSnapshot, StorageError> {
        let input = QuotaSnapshotInput {
            provider: parse_stored("provider", &self.provider)?,
            scope_key: self.scope_key,
            window_kind: parse_stored("window_kind", &self.window_kind)?,
            limit_kind: parse_stored("limit_kind", &self.limit_kind)?,
            window_duration_seconds: self.window_duration_seconds,
            used_percent: self.used_percent,
            resets_at: self.resets_at,
            observed_at: self.observed_at,
            availability: parse_stored("availability", &self.availability)?,
            source_version: self.source_version,
            quality: parse_stored("quality", &self.quality)?,
            source_sequence: Uuid::parse_str(&self.source_sequence).map_err(|_| {
                StorageError::InvalidStoredValue {
                    field: "source_sequence",
                }
            })?,
        };
        Ok(StoredSnapshot {
            id: self.id,
            snapshot: QuotaSnapshot::new(input)?,
        })
    }
}

fn parse_stored<T: FromStr>(field: &'static str, value: &str) -> Result<T, StorageError> {
    value
        .parse()
        .map_err(|_| StorageError::InvalidStoredValue { field })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Availability, LimitKind, Quality};
    use tempfile::TempDir;

    fn fixture_storage() -> (TempDir, Storage) {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::open(temp.path().join("nested/usage.sqlite3")).unwrap();
        (temp, storage)
    }

    fn snapshot(observed_at: i64, used_percent: f64) -> QuotaSnapshot {
        QuotaSnapshot::new(QuotaSnapshotInput {
            provider: Provider::Codex,
            scope_key: "subscription".into(),
            window_kind: WindowKind::Rolling5h,
            limit_kind: LimitKind::Limited,
            window_duration_seconds: Some(18_000),
            used_percent,
            resets_at: Some(1_800_000_000),
            observed_at,
            availability: Availability::Allowed,
            source_version: Some("0.154.0".into()),
            quality: Quality::Fresh,
            source_sequence: Uuid::new_v4(),
        })
        .unwrap()
    }

    fn create_v1_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE snapshots (
                    id INTEGER PRIMARY KEY,
                    provider TEXT NOT NULL CHECK (provider IN ('codex','claude')),
                    scope_key TEXT NOT NULL,
                    window_kind TEXT NOT NULL CHECK (window_kind IN ('rolling_5h','rolling_7d','spend','other')),
                    window_duration_seconds INTEGER,
                    used_percent REAL NOT NULL,
                    resets_at INTEGER,
                    observed_at INTEGER NOT NULL,
                    availability TEXT NOT NULL CHECK (availability IN ('allowed','blocked','unknown')),
                    source_version TEXT,
                    quality TEXT NOT NULL CHECK (quality IN ('fresh','partial')),
                    source_sequence TEXT NOT NULL,
                    CHECK (used_percent >= 0),
                    UNIQUE(provider, scope_key, window_kind, observed_at)
                 );
                 CREATE INDEX snapshots_latest
                    ON snapshots(provider, scope_key, window_kind, observed_at DESC);
                 CREATE TABLE provider_state (
                    provider TEXT PRIMARY KEY,
                    last_success_at INTEGER,
                    last_attempt_at INTEGER,
                    consecutive_failures INTEGER NOT NULL DEFAULT 0,
                    last_error_class TEXT
                 );
                 CREATE TABLE alert_state (
                    provider TEXT NOT NULL,
                    scope_key TEXT NOT NULL,
                    window_kind TEXT NOT NULL,
                    alert_kind TEXT NOT NULL,
                    cycle_key TEXT NOT NULL,
                    fired_at INTEGER NOT NULL,
                    PRIMARY KEY(provider, scope_key, window_kind, alert_kind, cycle_key)
                 );
                 INSERT INTO snapshots VALUES
                    (41, 'codex', 'subscription', 'rolling_5h', 18000, 12.5,
                     1800000000, 1700000000000, 'allowed', '0.154.0', 'fresh',
                     '00000000-0000-0000-0000-000000000041'),
                    (42, 'claude', 'subscription', 'rolling_7d', 604800, 33.25,
                     NULL, 1700000001000, 'unknown', NULL, 'partial',
                     '00000000-0000-0000-0000-000000000042');
                 INSERT INTO provider_state VALUES
                    ('codex', 1700000000000, 1700000002000, 2, 'timeout'),
                    ('claude', 1700000001000, 1700000001000, 0, NULL);
                 INSERT INTO alert_state VALUES
                    ('codex', 'subscription', 'rolling_5h', 'reset_soon',
                     '1800000000', 1700000003000);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
    }

    fn create_v2_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE snapshots (
                    id INTEGER PRIMARY KEY,
                    provider TEXT NOT NULL CHECK (provider IN ('codex','claude','github_copilot')),
                    scope_key TEXT NOT NULL,
                    window_kind TEXT NOT NULL CHECK (window_kind IN ('rolling_5h','rolling_7d','spend','monthly','other')),
                    limit_kind TEXT NOT NULL CHECK (limit_kind IN ('limited','unlimited')),
                    window_duration_seconds INTEGER,
                    used_percent REAL NOT NULL,
                    resets_at INTEGER,
                    observed_at INTEGER NOT NULL,
                    availability TEXT NOT NULL CHECK (availability IN ('allowed','blocked','unknown')),
                    source_version TEXT,
                    quality TEXT NOT NULL CHECK (quality IN ('fresh','partial')),
                    source_sequence TEXT NOT NULL,
                    CHECK (used_percent >= 0),
                    UNIQUE(provider, scope_key, window_kind, observed_at)
                 );
                 CREATE INDEX snapshots_latest
                    ON snapshots(provider, scope_key, window_kind, observed_at DESC);
                 CREATE TABLE provider_state (
                    provider TEXT PRIMARY KEY,
                    last_success_at INTEGER,
                    last_attempt_at INTEGER,
                    consecutive_failures INTEGER NOT NULL DEFAULT 0,
                    last_error_class TEXT
                 );
                 CREATE TABLE alert_state (
                    provider TEXT NOT NULL,
                    scope_key TEXT NOT NULL,
                    window_kind TEXT NOT NULL,
                    alert_kind TEXT NOT NULL,
                    cycle_key TEXT NOT NULL,
                    fired_at INTEGER NOT NULL,
                    PRIMARY KEY(provider, scope_key, window_kind, alert_kind, cycle_key)
                 );
                 INSERT INTO snapshots VALUES
                    (51, 'codex', 'subscription', 'rolling_5h', 'limited', 18000,
                     12.5, 1800000000, 1700000000000, 'allowed', '0.154.0', 'fresh',
                     '00000000-0000-0000-0000-000000000051'),
                    (52, 'claude', 'subscription', 'other', 'limited', NULL,
                     33.25, NULL, 1700000001000, 'unknown', NULL, 'partial',
                     '00000000-0000-0000-0000-000000000052'),
                    (53, 'github_copilot', 'premium_interactions', 'monthly', 'unlimited', NULL,
                     0.0, 1800000100, 1700000002000, 'allowed', '1.0.13', 'fresh',
                     '00000000-0000-0000-0000-000000000053');
                 INSERT INTO provider_state VALUES
                    ('github_copilot', 1700000002000, 1700000003000, 3, 'cli_error');
                 INSERT INTO alert_state VALUES
                    ('github_copilot', 'premium_interactions', 'monthly', 'low_remaining',
                     '1800000100', 1700000004000);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
    }

    #[derive(Debug, PartialEq)]
    struct SnapshotProjection {
        id: i64,
        provider: String,
        scope_key: String,
        window_kind: String,
        limit_kind: String,
        window_duration_seconds: Option<i64>,
        used_percent: f64,
        resets_at: Option<i64>,
        observed_at: i64,
        availability: String,
        source_version: Option<String>,
        quality: String,
        source_sequence: String,
    }

    #[derive(Debug, PartialEq)]
    struct LegacySnapshotProjection {
        id: i64,
        provider: String,
        scope_key: String,
        window_kind: String,
        window_duration_seconds: Option<i64>,
        used_percent: f64,
        resets_at: Option<i64>,
        observed_at: i64,
        availability: String,
        source_version: Option<String>,
        quality: String,
        source_sequence: String,
    }

    fn snapshot_projection(connection: &Connection) -> Vec<SnapshotProjection> {
        let mut statement = connection
            .prepare(
                "SELECT id, provider, scope_key, window_kind, limit_kind,
                        window_duration_seconds, used_percent, resets_at, observed_at,
                        availability, source_version, quality, source_sequence
                 FROM snapshots ORDER BY id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok(SnapshotProjection {
                    id: row.get(0)?,
                    provider: row.get(1)?,
                    scope_key: row.get(2)?,
                    window_kind: row.get(3)?,
                    limit_kind: row.get(4)?,
                    window_duration_seconds: row.get(5)?,
                    used_percent: row.get(6)?,
                    resets_at: row.get(7)?,
                    observed_at: row.get(8)?,
                    availability: row.get(9)?,
                    source_version: row.get(10)?,
                    quality: row.get(11)?,
                    source_sequence: row.get(12)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn legacy_snapshot_projection(connection: &Connection) -> Vec<LegacySnapshotProjection> {
        let mut statement = connection
            .prepare(
                "SELECT id, provider, scope_key, window_kind,
                        window_duration_seconds, used_percent, resets_at, observed_at,
                        availability, source_version, quality, source_sequence
                 FROM snapshots ORDER BY id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok(LegacySnapshotProjection {
                    id: row.get(0)?,
                    provider: row.get(1)?,
                    scope_key: row.get(2)?,
                    window_kind: row.get(3)?,
                    window_duration_seconds: row.get(4)?,
                    used_percent: row.get(5)?,
                    resets_at: row.get(6)?,
                    observed_at: row.get(7)?,
                    availability: row.get(8)?,
                    source_version: row.get(9)?,
                    quality: row.get(10)?,
                    source_sequence: row.get(11)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn with_default_limit(rows: &[LegacySnapshotProjection]) -> Vec<SnapshotProjection> {
        rows.iter()
            .map(|row| SnapshotProjection {
                id: row.id,
                provider: row.provider.clone(),
                scope_key: row.scope_key.clone(),
                window_kind: row.window_kind.clone(),
                limit_kind: "limited".into(),
                window_duration_seconds: row.window_duration_seconds,
                used_percent: row.used_percent,
                resets_at: row.resets_at,
                observed_at: row.observed_at,
                availability: row.availability.clone(),
                source_version: row.source_version.clone(),
                quality: row.quality.clone(),
                source_sequence: row.source_sequence.clone(),
            })
            .collect()
    }

    type ProviderStateProjection = (String, Option<i64>, Option<i64>, i64, Option<String>);
    type AlertStateProjection = (String, String, String, String, String, i64);

    fn provider_state_projection(connection: &Connection) -> Vec<ProviderStateProjection> {
        let mut statement = connection
            .prepare(
                "SELECT provider, last_success_at, last_attempt_at,
                        consecutive_failures, last_error_class
                 FROM provider_state ORDER BY provider",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn alert_state_projection(connection: &Connection) -> Vec<AlertStateProjection> {
        let mut statement = connection
            .prepare(
                "SELECT provider, scope_key, window_kind, alert_kind, cycle_key, fired_at
                 FROM alert_state
                 ORDER BY provider, scope_key, window_kind, alert_kind, cycle_key",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn schema_object_sql(connection: &Connection, object_type: &str, name: &str) -> String {
        connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = ?1 AND name = ?2",
                params![object_type, name],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn migrations_are_idempotent_and_enable_wal() {
        let (_temp, storage) = fixture_storage();
        Storage::open(storage.database_path()).unwrap();
        let connection = storage.connect().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn new_v3_schema_accepts_only_the_four_exact_provider_values() {
        let (_temp, storage) = fixture_storage();
        let connection = storage.connect().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);

        for (index, provider) in [
            Provider::Codex,
            Provider::Claude,
            Provider::GitHubCopilot,
            Provider::GoogleAntigravity,
        ]
        .into_iter()
        .enumerate()
        {
            connection
                .execute(
                    "INSERT INTO snapshots (
                        provider, scope_key, window_kind, limit_kind, used_percent,
                        observed_at, availability, quality, source_sequence
                     ) VALUES (?1, 'scope', 'other', 'limited', 0, ?2, 'allowed',
                               'fresh', ?3)",
                    params![
                        provider.as_str(),
                        1_700_000_000_000_i64 + index as i64,
                        Uuid::new_v4().to_string()
                    ],
                )
                .unwrap();
        }

        for rejected in ["gemini", "antigravity", "google-antigravity", "unknown"] {
            let result = connection.execute(
                "INSERT INTO snapshots (
                    provider, scope_key, window_kind, limit_kind, used_percent,
                    observed_at, availability, quality, source_sequence
                 ) VALUES (?1, 'scope', 'other', 'limited', 0, 1800000000000,
                           'allowed', 'fresh', ?2)",
                params![rejected, Uuid::new_v4().to_string()],
            );
            assert!(result.is_err(), "provider {rejected} unexpectedly passed");
        }
    }

    #[test]
    fn insert_and_latest_round_trip_preserves_normalized_values() {
        let (_temp, storage) = fixture_storage();
        storage.insert_snapshots(&[snapshot(1_000, 10.5)]).unwrap();
        storage.insert_snapshots(&[snapshot(2_000, 22.25)]).unwrap();

        let stored = storage
            .latest_snapshot(Provider::Codex, "subscription", WindowKind::Rolling5h)
            .unwrap()
            .unwrap();
        assert_eq!(stored.snapshot.as_input().used_percent, 22.25);
        assert_eq!(stored.snapshot.remaining_percent(), 77.75);
        assert_eq!(storage.latest_snapshots().unwrap().len(), 1);
    }

    #[test]
    fn github_copilot_monthly_limit_kinds_round_trip() {
        let (_temp, storage) = fixture_storage();
        let rows = [
            QuotaSnapshot::new(QuotaSnapshotInput {
                provider: Provider::GitHubCopilot,
                scope_key: "premium_interactions".into(),
                window_kind: WindowKind::Monthly,
                limit_kind: LimitKind::Limited,
                window_duration_seconds: None,
                used_percent: 25.5,
                resets_at: Some(1_800_000_000),
                observed_at: 1_700_000_000_000,
                availability: Availability::Allowed,
                source_version: Some("1.0.13".into()),
                quality: Quality::Fresh,
                source_sequence: Uuid::new_v4(),
            })
            .unwrap(),
            QuotaSnapshot::new(QuotaSnapshotInput {
                provider: Provider::GitHubCopilot,
                scope_key: "chat".into(),
                window_kind: WindowKind::Monthly,
                limit_kind: LimitKind::Unlimited,
                window_duration_seconds: None,
                used_percent: 0.0,
                resets_at: None,
                observed_at: 1_700_000_000_000,
                availability: Availability::Allowed,
                source_version: Some("1.0.13".into()),
                quality: Quality::Partial,
                source_sequence: Uuid::new_v4(),
            })
            .unwrap(),
        ];

        storage.insert_snapshots(&rows).unwrap();
        let stored = storage.latest_snapshots().unwrap();
        assert_eq!(stored.len(), 2);
        assert!(stored.iter().any(|row| {
            let row = row.snapshot.as_input();
            row.scope_key == "premium_interactions" && row.limit_kind == LimitKind::Limited
        }));
        assert!(stored.iter().any(|row| {
            let row = row.snapshot.as_input();
            row.scope_key == "chat" && row.limit_kind == LimitKind::Unlimited
        }));
    }

    #[test]
    fn v1_migration_is_lossless_and_reopen_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        create_v1_database(&path);
        let (expected_snapshots, expected_provider_state, expected_alert_state) = {
            let connection = Connection::open(&path).unwrap();
            let legacy = legacy_snapshot_projection(&connection);
            (
                with_default_limit(&legacy),
                provider_state_projection(&connection),
                alert_state_projection(&connection),
            )
        };

        let storage = Storage::open(&path).unwrap();
        let migrated_connection = storage.connect().unwrap();
        assert_eq!(
            snapshot_projection(&migrated_connection),
            expected_snapshots
        );
        assert_eq!(
            provider_state_projection(&migrated_connection),
            expected_provider_state
        );
        assert_eq!(
            alert_state_projection(&migrated_connection),
            expected_alert_state
        );
        drop(migrated_connection);
        let snapshots = storage.recent_history(10).unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots.iter().map(|row| row.id).collect::<Vec<_>>(),
            [42, 41]
        );
        assert!(
            snapshots
                .iter()
                .all(|row| row.snapshot.as_input().limit_kind == LimitKind::Limited)
        );
        assert_eq!(snapshots[0].snapshot.as_input().provider, Provider::Claude);
        assert_eq!(snapshots[0].snapshot.as_input().resets_at, None);
        assert_eq!(snapshots[1].snapshot.as_input().used_percent, 12.5);

        let codex = storage.provider_state(Provider::Codex).unwrap().unwrap();
        assert_eq!(codex.last_success_at, Some(1_700_000_000_000));
        assert_eq!(codex.last_attempt_at, Some(1_700_000_002_000));
        assert_eq!(codex.consecutive_failures, 2);
        assert_eq!(codex.last_error_class.as_deref(), Some("timeout"));
        let claude = storage.provider_state(Provider::Claude).unwrap().unwrap();
        assert_eq!(claude.last_success_at, Some(1_700_000_001_000));
        assert_eq!(claude.last_attempt_at, Some(1_700_000_001_000));
        assert_eq!(claude.consecutive_failures, 0);
        assert_eq!(claude.last_error_class, None);
        let alerts = storage.recent_alerts(10).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].provider, Provider::Codex);
        assert_eq!(alerts[0].scope_key, "subscription");
        assert_eq!(alerts[0].window_kind, WindowKind::Rolling5h);
        assert_eq!(alerts[0].alert_kind, "reset_soon");
        assert_eq!(alerts[0].cycle_key, "1800000000");
        assert_eq!(alerts[0].fired_at, 1_700_000_003_000);

        drop(storage);
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(reopened.recent_history(10).unwrap().len(), 2);
        assert_eq!(reopened.recent_alerts(10).unwrap().len(), 1);
        let connection = reopened.connect().unwrap();
        assert_eq!(snapshot_projection(&connection), expected_snapshots);
        assert_eq!(
            provider_state_projection(&connection),
            expected_provider_state
        );
        assert_eq!(alert_state_projection(&connection), expected_alert_state);
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'snapshots_latest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let duplicate_result = connection.execute(
            "INSERT INTO snapshots (
                id, provider, scope_key, window_kind, limit_kind,
                window_duration_seconds, used_percent, resets_at, observed_at,
                availability, source_version, quality, source_sequence
             )
             SELECT 99, provider, scope_key, window_kind, limit_kind,
                    window_duration_seconds, used_percent, resets_at, observed_at,
                    availability, source_version, quality,
                    '00000000-0000-0000-0000-000000000099'
             FROM snapshots WHERE id = 41",
            [],
        );
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(index_count, 1);
        assert!(duplicate_result.is_err());
    }

    #[test]
    fn failed_v1_migration_rolls_back_the_original_schema_and_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        create_v1_database(&path);
        let connection = Connection::open(&path).unwrap();
        let before_snapshots = legacy_snapshot_projection(&connection);
        let before_provider_state = provider_state_projection(&connection);
        let before_alert_state = alert_state_projection(&connection);
        let before_table_sql = schema_object_sql(&connection, "table", "snapshots");
        let before_index_sql = schema_object_sql(&connection, "index", "snapshots_latest");
        connection
            .execute("CREATE TABLE snapshots_v1 (blocker INTEGER)", [])
            .unwrap();
        drop(connection);

        assert!(Storage::open(&path).is_err());

        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let snapshot_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'snapshots_latest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut statement = connection.prepare("PRAGMA table_info(snapshots)").unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(snapshot_count, before_snapshots.len() as i64);
        assert_eq!(legacy_snapshot_projection(&connection), before_snapshots);
        assert_eq!(
            provider_state_projection(&connection),
            before_provider_state
        );
        assert_eq!(alert_state_projection(&connection), before_alert_state);
        assert_eq!(
            schema_object_sql(&connection, "table", "snapshots"),
            before_table_sql
        );
        assert_eq!(
            schema_object_sql(&connection, "index", "snapshots_latest"),
            before_index_sql
        );
        assert_eq!(index_count, 1);
        assert!(!columns.iter().any(|column| column == "limit_kind"));
    }

    #[test]
    fn v2_migration_is_lossless_preserves_supporting_state_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        create_v2_database(&path);
        let (before, expected_provider_state, expected_alert_state) = {
            let connection = Connection::open(&path).unwrap();
            (
                snapshot_projection(&connection),
                provider_state_projection(&connection),
                alert_state_projection(&connection),
            )
        };

        let storage = Storage::open(&path).unwrap();
        let connection = storage.connect().unwrap();
        assert_eq!(snapshot_projection(&connection), before);
        assert_eq!(
            provider_state_projection(&connection),
            expected_provider_state
        );
        assert_eq!(alert_state_projection(&connection), expected_alert_state);
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'snapshots_latest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(index_count, 1);
        drop(connection);

        let copilot = storage
            .provider_state(Provider::GitHubCopilot)
            .unwrap()
            .unwrap();
        assert_eq!(copilot.last_success_at, Some(1_700_000_002_000));
        assert_eq!(copilot.last_attempt_at, Some(1_700_000_003_000));
        assert_eq!(copilot.consecutive_failures, 3);
        assert_eq!(copilot.last_error_class.as_deref(), Some("cli_error"));
        let alerts = storage.recent_alerts(10).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].provider, Provider::GitHubCopilot);
        assert_eq!(alerts[0].scope_key, "premium_interactions");
        assert_eq!(alerts[0].window_kind, WindowKind::Monthly);
        assert_eq!(alerts[0].alert_kind, "low_remaining");

        drop(storage);
        let reopened = Storage::open(&path).unwrap();
        let connection = reopened.connect().unwrap();
        assert_eq!(snapshot_projection(&connection), before);
        assert_eq!(
            provider_state_projection(&connection),
            expected_provider_state
        );
        assert_eq!(alert_state_projection(&connection), expected_alert_state);
        let duplicate_result = connection.execute(
            "INSERT INTO snapshots (
                id, provider, scope_key, window_kind, limit_kind,
                window_duration_seconds, used_percent, resets_at, observed_at,
                availability, source_version, quality, source_sequence
             )
             SELECT 99, provider, scope_key, window_kind, limit_kind,
                    window_duration_seconds, used_percent, resets_at, observed_at,
                    availability, source_version, quality,
                    '00000000-0000-0000-0000-000000000099'
             FROM snapshots WHERE id = 51",
            [],
        );
        assert!(duplicate_result.is_err());
    }

    #[test]
    fn failed_v2_migration_rolls_back_the_original_schema_and_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        create_v2_database(&path);
        let connection = Connection::open(&path).unwrap();
        let before = snapshot_projection(&connection);
        let before_provider_state = provider_state_projection(&connection);
        let before_alert_state = alert_state_projection(&connection);
        let before_table_sql = schema_object_sql(&connection, "table", "snapshots");
        let before_index_sql = schema_object_sql(&connection, "index", "snapshots_latest");
        connection
            .execute("CREATE TABLE snapshots_v2 (blocker INTEGER)", [])
            .unwrap();
        drop(connection);

        assert!(Storage::open(&path).is_err());

        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'snapshots_latest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut statement = connection.prepare("PRAGMA table_info(snapshots)").unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(snapshot_projection(&connection), before);
        assert_eq!(
            provider_state_projection(&connection),
            before_provider_state
        );
        assert_eq!(alert_state_projection(&connection), before_alert_state);
        assert_eq!(
            schema_object_sql(&connection, "table", "snapshots"),
            before_table_sql
        );
        assert_eq!(
            schema_object_sql(&connection, "index", "snapshots_latest"),
            before_index_sql
        );
        assert_eq!(index_count, 1);
        assert!(columns.iter().any(|column| column == "limit_kind"));
    }

    #[test]
    fn newer_schema_versions_are_rejected_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 4).unwrap();
        drop(connection);

        assert!(matches!(
            Storage::open(&path),
            Err(StorageError::UnsupportedSchemaVersion {
                found: 4,
                supported: 3
            })
        ));
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }

    #[test]
    fn retention_prunes_only_rows_older_than_cutoff() {
        let (_temp, storage) = fixture_storage();
        storage
            .insert_snapshots(&[snapshot(1_000, 10.0), snapshot(2_000, 20.0)])
            .unwrap();
        assert_eq!(storage.prune_before(2_000).unwrap(), 1);

        let connection = storage.connect().unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            storage.latest_snapshots().unwrap()[0]
                .snapshot
                .as_input()
                .observed_at,
            2_000
        );
    }

    #[test]
    fn schema_contains_only_permitted_normalized_snapshot_fields() {
        let (_temp, storage) = fixture_storage();
        let connection = storage.connect().unwrap();
        let mut statement = connection.prepare("PRAGMA table_info(snapshots)").unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            columns,
            [
                "id",
                "provider",
                "scope_key",
                "window_kind",
                "limit_kind",
                "window_duration_seconds",
                "used_percent",
                "resets_at",
                "observed_at",
                "availability",
                "source_version",
                "quality",
                "source_sequence",
            ]
        );
        let schema: String = connection
            .query_row(
                "SELECT group_concat(sql, ' ') FROM sqlite_schema WHERE sql IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let schema = schema.to_ascii_lowercase();
        for forbidden in [
            "token",
            "credential",
            "raw_json",
            "raw_payload",
            "rawjson",
            "rawpayload",
            "oauth",
            "access_token",
            "cookie",
            "prompt",
            "transcript",
            "repository",
            "project",
            "username",
            "account_id",
            "account_identifier",
            "usedrequests",
            "used_requests",
            "entitlementrequests",
            "entitlement_requests",
        ] {
            assert!(!schema.contains(forbidden), "found forbidden {forbidden}");
        }
    }

    #[test]
    fn storage_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Storage>();
    }
}
