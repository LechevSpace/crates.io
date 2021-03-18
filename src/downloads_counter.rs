use crate::App;
use anyhow::Error;
use dashmap::{DashMap, SharedValue};
use diesel::{pg::upsert::excluded, prelude::*};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

/// crates.io receives a lot of download requests, and we can't execute a write query to the
/// database during each connection for performance reasons. To reduce the write load, this struct
/// collects the pending updates from the current process and writes in batch.
///
/// To avoid locking the whole data structure behind a RwLock, which could potentially delay
/// requests, this uses the dashmap crate. A DashMap has the same public API as an HashMap, but
/// stores the items into `num_cpus()*4` individually locked shards. This approach reduces the
/// likelyhood of a request encountering a locked shard.
///
/// Persisting the download counts in the database also takes advantage of the inner sharding of
/// DashMaps: to avoid locking all the download requests at the same time each iteration only
/// persists a single shard at the time.
///
/// The disadvantage of this approach is that download counts are stored in memory until they're
/// persisted, so it's possible to lose some of them if the process exits ungracefully. While
/// that's far from ideal, the advantage of batching database updates far outweights potentially
/// losing some download counts.
#[derive(Debug)]
pub struct DownloadsCounter {
    /// Inner storage for the download counts.
    inner: DashMap<i32, AtomicUsize>,
    /// Index of the next shard that should be persisted by `persist_next_shard`.
    shard_idx: AtomicUsize,
    /// Number of downloads that are not yet persisted on the database. This is just used as a
    /// metric included in log lines, and it's not guaranteed to be accurate.
    pending_count: AtomicI64,
}

impl DownloadsCounter {
    pub(crate) fn new() -> Self {
        Self {
            inner: DashMap::new(),
            shard_idx: AtomicUsize::new(0),
            pending_count: AtomicI64::new(0),
        }
    }

    pub(crate) fn increment(&self, version_id: i32) {
        self.pending_count.fetch_add(1, Ordering::SeqCst);

        if let Some(counter) = self.inner.get(&version_id) {
            // The version is already recorded in the DashMap, so we don't need to lock the whole
            // shard in write mode. The shard is instead locked in read mode, which allows an
            // unbounded number of readers as long as there are no write locks.
            counter.value().fetch_add(1, Ordering::SeqCst);
        } else {
            // The version is not in the DashMap, so we need to lock the whole shard in write mode
            // and insert the version into it. This has worse performance than the above case.
            self.inner
                .entry(version_id)
                .and_modify(|counter| {
                    // Handle the version being inserted by another thread while we were waiting
                    // for the write lock on the shard.
                    counter.fetch_add(1, Ordering::SeqCst);
                })
                .or_insert_with(|| AtomicUsize::new(1));
        }
    }

    pub fn persist_all_shards(&self, app: &App) -> Result<PersistStats, Error> {
        let conn = app.primary_database.get()?;
        self.persist_all_shards_with_conn(&conn)
    }

    pub fn persist_next_shard(&self, app: &App) -> Result<PersistStats, Error> {
        let conn = app.primary_database.get()?;
        self.persist_next_shard_with_conn(&conn)
    }

    fn persist_all_shards_with_conn(&self, conn: &PgConnection) -> Result<PersistStats, Error> {
        let mut stats = PersistStats::default();
        for shard in self.inner.shards() {
            let shard = std::mem::take(&mut *shard.write());
            stats = stats.merge(self.persist_shard(&conn, shard)?);
        }

        Ok(stats)
    }

    fn persist_next_shard_with_conn(&self, conn: &PgConnection) -> Result<PersistStats, Error> {
        // Replace the next shard in the ring with an empty HashMap (clearing it), and return the
        // previous contents for processing. The fetch_add method wraps around on overflow, so it's
        // fine to keep incrementing it without resetting.
        let shards = self.inner.shards();
        let idx = self.shard_idx.fetch_add(1, Ordering::SeqCst) % shards.len();
        let shard = std::mem::take(&mut *shards[idx].write());

        let mut stats = self.persist_shard(&conn, shard)?;
        stats.shard = Some(idx);
        Ok(stats)
    }

    fn persist_shard(
        &self,
        conn: &PgConnection,
        shard: HashMap<i32, SharedValue<AtomicUsize>>,
    ) -> Result<PersistStats, Error> {
        use crate::schema::version_downloads::dsl::*;

        let mut counted_downloads = 0;
        let mut counted_versions = 0;
        let mut to_insert = Vec::new();
        for (key, atomic) in shard.iter() {
            let count = atomic.get().load(Ordering::SeqCst);
            counted_downloads += count;
            counted_versions += 1;

            to_insert.push((*key, count));
        }

        if !to_insert.is_empty() {
            // The rows we're about to insert need to be sorted to avoid deadlocks when multiple
            // instances of crates.io are running at the same time.
            //
            // In PostgreSQL a transaction modifying a row locks that row until the transaction is
            // committed. Multiple transactions inserting rows into a table could end up
            // deadlocking each other though: PostgreSQL will detect that deadlock, abort one of
            // the transactions and allow the other one to continue. We don't want that to happen,
            // as we'd lose the downloads from the aborted transaction.
            //
            // Ensuring the rows are inserted in a consistent order (in our case by sorting them by
            // the version ID) will prevent deadlocks from occuring. For more information:
            //
            //     https://www.postgresql.org/docs/11/explicit-locking.html#LOCKING-DEADLOCKS
            //
            to_insert.sort_by_key(|(key, _)| *key);

            let to_insert = to_insert
                .into_iter()
                .map(|(key, count)| (version_id.eq(key), downloads.eq(count as i32)))
                .collect::<Vec<_>>();

            diesel::insert_into(version_downloads)
                .values(&to_insert)
                .on_conflict((version_id, date))
                .do_update()
                .set(downloads.eq(downloads + excluded(downloads)))
                .execute(conn)?;
        }

        let old_pending = self
            .pending_count
            .fetch_sub(counted_downloads as i64, Ordering::SeqCst);

        Ok(PersistStats {
            shard: None,
            counted_downloads,
            counted_versions,
            pending_downloads: old_pending - counted_downloads as i64,
        })
    }

    pub fn shards_count(&self) -> usize {
        self.inner.shards().len()
    }
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct PersistStats {
    shard: Option<usize>,
    counted_downloads: usize,
    counted_versions: usize,
    pending_downloads: i64,
}

impl PersistStats {
    fn merge(self, other: PersistStats) -> Self {
        Self {
            shard: if self.shard == other.shard {
                other.shard
            } else {
                None
            },
            counted_downloads: self.counted_downloads + other.counted_downloads,
            counted_versions: self.counted_versions + other.counted_versions,
            pending_downloads: other.pending_downloads,
        }
    }

    pub fn log(&self) {
        println!(
            "downloads_counter shard={} counted_versions={} counted_downloads={} pending_downloads={}",
            self.shard.map(|s| s.to_string()).unwrap_or_else(|| "all".into()),
            self.counted_versions,
            self.counted_downloads,
            self.pending_downloads,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Crate, NewCrate, NewUser, NewVersion, User};
    use diesel::PgConnection;
    use semver::Version;

    #[test]
    fn test_increment_and_persist_all() {
        let counter = DownloadsCounter::new();
        let conn = crate::db::test_conn();
        let mut state = State::new(&conn);

        let v1 = state.new_version(&conn);
        let v2 = state.new_version(&conn);
        let v3 = state.new_version(&conn);

        // Add 15 downloads between v1 and v2, and no downloads for v3.
        for _ in 0..10 {
            counter.increment(v1);
        }
        for _ in 0..5 {
            counter.increment(v2);
        }
        assert_eq!(15, counter.pending_count.load(Ordering::SeqCst));

        // Persist everything to the database
        let stats = counter
            .persist_all_shards_with_conn(&conn)
            .expect("failed to persist all shards");

        // Ensure the stats are accurate
        assert_eq!(
            stats,
            PersistStats {
                shard: None,
                counted_downloads: 15,
                counted_versions: 2,
                pending_downloads: 0,
            }
        );

        // Ensure the download counts in the database are what we expect.
        state.assert_downloads_count(&conn, v1, 10);
        state.assert_downloads_count(&conn, v2, 5);
        state.assert_downloads_count(&conn, v3, 0);
    }

    #[test]
    fn test_increment_and_persist_shard() {
        let counter = DownloadsCounter::new();
        let conn = crate::db::test_conn();
        let mut state = State::new(&conn);

        let v1 = state.new_version(&conn);
        let v1_shard = counter.inner.determine_map(&v1);

        // For this test to work we need the two versions to be stored in different shards.
        let mut v2 = state.new_version(&conn);
        while counter.inner.determine_map(&v2) == v1_shard {
            v2 = state.new_version(&conn);
        }
        let v2_shard = counter.inner.determine_map(&v2);

        // Add 15 downloads between v1 and v2.
        for _ in 0..10 {
            counter.increment(v1);
        }
        for _ in 0..5 {
            counter.increment(v2);
        }
        assert_eq!(15, counter.pending_count.load(Ordering::SeqCst));

        // Persist one shard at the time and ensure the stats returned for each shard are expected.
        let mut pending = 15;
        for shard in 0..counter.shards_count() {
            let stats = counter
                .persist_next_shard_with_conn(&conn)
                .expect("failed to persist shard");

            if shard == v1_shard {
                pending -= 10;
                assert_eq!(
                    stats,
                    PersistStats {
                        shard: Some(shard),
                        counted_downloads: 10,
                        counted_versions: 1,
                        pending_downloads: pending,
                    }
                );
                state.assert_downloads_count(&conn, v1, 10);
            } else if shard == v2_shard {
                pending -= 5;
                assert_eq!(
                    stats,
                    PersistStats {
                        shard: Some(shard),
                        counted_downloads: 5,
                        counted_versions: 1,
                        pending_downloads: pending,
                    }
                );
                state.assert_downloads_count(&conn, v2, 5);
            } else {
                assert_eq!(
                    stats,
                    PersistStats {
                        shard: Some(shard),
                        counted_downloads: 0,
                        counted_versions: 0,
                        pending_downloads: pending,
                    }
                );
            };
        }
        assert_eq!(pending, 0);

        // Finally ensure that the download counts in the database are what we expect.
        state.assert_downloads_count(&conn, v1, 10);
        state.assert_downloads_count(&conn, v2, 5);
    }

    struct State {
        user: User,
        krate: Crate,
        next_version: u32,
    }

    impl State {
        fn new(conn: &PgConnection) -> Self {
            let user = NewUser {
                gh_id: 0,
                gh_login: "ghost",
                ..NewUser::default()
            }
            .create_or_update(None, conn)
            .expect("failed to create user");

            let krate = NewCrate {
                name: "foo",
                ..NewCrate::default()
            }
            .create_or_update(conn, user.id, None)
            .expect("failed to create crate");

            Self {
                user,
                krate,
                next_version: 1,
            }
        }

        fn new_version(&mut self, conn: &PgConnection) -> i32 {
            let version = NewVersion::new(
                self.krate.id,
                &Version::parse(&format!("{}.0.0", self.next_version)).unwrap(),
                &HashMap::new(),
                None,
                None,
                0,
                self.user.id,
            )
            .expect("failed to create version")
            .save(conn, &[], "ghost@example.com")
            .expect("failed to save version");

            self.next_version += 1;
            version.id
        }

        fn assert_downloads_count(&self, conn: &PgConnection, version: i32, expected: i64) {
            use crate::schema::version_downloads::dsl::*;
            use diesel::dsl::*;

            let actual: Option<i64> = version_downloads
                .select(sum(downloads))
                .filter(version_id.eq(version))
                .first(conn)
                .unwrap();
            assert_eq!(actual.unwrap_or(0), expected);
        }
    }
}
