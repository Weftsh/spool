//! Where a build log lives, and how it is put back together.
//!
//! A log is written twice, and that is deliberate rather than wasteful.
//!
//! While the job runs the runner uploads **chunks** — one object per
//! flush — because a log a reader cannot see until the build ends is a
//! log they watch a spinner instead of. When the job ends it uploads the
//! **whole file** once, and that object is authoritative: a chunk POST
//! that failed twice is dropped by the runner rather than retried
//! forever, so the chunk sequence can have holes in it and the final
//! upload is the copy that does not.
//!
//! [`read_log`] encodes exactly that precedence — final object if there
//! is one, chunks in order if there is not — so no caller has to know
//! which half of a job's life it is looking at.
//!
//! Everything here is in the store rather than in Postgres because a
//! build log is megabytes of text that nobody ever queries on, and the
//! bucket has a lifecycle rule (`expire-ci-logs`) that ages it out.
//! `workflow_jobs.log_chunks` is the only part the database keeps, and
//! it exists so a reader knows how many objects to ask for.

use stratum_store::{ObjectStore, PutCond};

/// The most one chunk may carry. The runner flushes at 64 KiB, so this
/// is four times its own bound — big enough that a legitimate flush
/// never trips it, small enough that a hostile client cannot make one
/// request cost a lot of memory.
pub const MAX_CHUNK: usize = 256 * 1024;

/// The most a complete log may be. Past this the job's own output is the
/// problem, and truncating is a better answer than refusing to record a
/// verdict at all — but that decision belongs to the runner, which knows
/// what it produced; here it is simply a refusal.
pub const MAX_LOG: usize = 16 * 1024 * 1024;

/// One flush of a running job's output.
///
/// The sequence is zero-padded so a plain lexicographic LIST comes back
/// in the order the chunks were produced — `10` sorting before `9` is
/// the classic way a reassembled log ends up scrambled, and it does not
/// announce itself.
pub fn chunk_key(run: &str, job: &str, attempt: i64, seq: i32) -> String {
    format!("ci/logs/{run}/{job}/{attempt}/{seq:06}.txt")
}

/// The complete log, uploaded once when the job ends.
///
/// Not keyed by attempt: a retry replaces the previous attempt's log
/// because what a reader wants beside a job is the log of the run that
/// produced its verdict, not an archive of the ones that did not.
pub fn log_key(run: &str, job: &str) -> String {
    format!("ci/logs/{run}/{job}/log.txt")
}

pub fn put_chunk(
    store: &ObjectStore,
    run: &str,
    job: &str,
    attempt: i64,
    seq: i32,
    text: &str,
) -> Result<(), String> {
    store
        .put(
            &chunk_key(run, job, attempt, seq),
            text.as_bytes(),
            PutCond::None,
        )
        .map_err(|e| format!("write log chunk: {e}"))
}

pub fn put_log(store: &ObjectStore, run: &str, job: &str, text: &str) -> Result<(), String> {
    store
        .put(&log_key(run, job), text.as_bytes(), PutCond::None)
        .map_err(|e| format!("write log: {e}"))
}

/// The log as it stands: complete if the job has uploaded it, otherwise
/// the chunks so far.
///
/// A chunk that is missing is skipped rather than failing the read. The
/// runner drops a chunk it could not POST twice, and the alternative —
/// answer 500 because sequence 7 is not there — turns one lost packet
/// into a page that will not load, for a log whose remaining 200 chunks
/// are sitting right there.
pub fn read_log(
    store: &ObjectStore,
    run: &str,
    job: &str,
    attempt: i64,
    chunks: i32,
) -> Result<String, String> {
    match store.get(&log_key(run, job)) {
        Ok(bytes) => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.contains("HTTP 404") => {}
        Err(e) => return Err(e),
    }
    let mut out = String::new();
    for seq in 1..=chunks {
        if let Ok(bytes) = store.get(&chunk_key(run, job, attempt, seq)) {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    Ok(out)
}

/// Drop this attempt's chunks once the whole log has been stored.
///
/// Best effort on purpose: the chunks are now redundant, the bucket's
/// lifecycle rule sweeps whatever is left, and failing a runner's
/// `finish` because a DELETE did not go through would throw away the
/// verdict — the one thing in the request that cannot be reconstructed.
pub fn delete_chunks(store: &ObjectStore, run: &str, job: &str, attempt: i64) {
    let prefix = format!("ci/logs/{run}/{job}/{attempt}/");
    let Ok(keys) = store.list(&prefix) else {
        return;
    };
    for (key, _) in keys {
        let _ = store.delete(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The zero-padding is the whole reason the key looks like this: a
    /// LIST is lexicographic, and `10` before `9` scrambles a log
    /// silently.
    #[test]
    fn chunk_keys_sort_in_the_order_they_were_produced() {
        let mut keys: Vec<String> = [9, 10, 1, 100]
            .iter()
            .map(|&s| chunk_key("R", "J", 1, s))
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "ci/logs/R/J/1/000001.txt",
                "ci/logs/R/J/1/000009.txt",
                "ci/logs/R/J/1/000010.txt",
                "ci/logs/R/J/1/000100.txt",
            ]
        );
        // The attempt is in the chunk path and not in the final log's:
        // a retry's chunks are separate, its finished log replaces.
        assert_eq!(chunk_key("R", "J", 2, 1), "ci/logs/R/J/2/000001.txt");
        assert_eq!(log_key("R", "J"), "ci/logs/R/J/log.txt");
    }

    /// Everything the store side does, against a real bucket: chunks
    /// concatenate in order, a hole is skipped rather than fatal, the
    /// final object wins the moment it exists, and deleting the chunks
    /// does not take the final log with them.
    #[test]
    fn the_final_log_wins_over_the_chunks_and_a_hole_is_not_fatal() {
        let minio = stratum_testkit::Minio::shared();
        let bucket = minio.bucket("wf-logs");
        let store = ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);

        // Nothing uploaded at all: an empty log, not an error. A job that
        // has produced no output yet is normal.
        assert_eq!(read_log(&store, "R", "J", 1, 0).unwrap(), "");

        put_chunk(&store, "R", "J", 1, 1, "first\n").unwrap();
        put_chunk(&store, "R", "J", 1, 3, "third\n").unwrap();
        assert_eq!(
            read_log(&store, "R", "J", 1, 3).unwrap(),
            "first\nthird\n",
            "chunk 2 never arrived; the rest still reads"
        );

        put_log(&store, "R", "J", "first\nsecond\nthird\n").unwrap();
        assert_eq!(
            read_log(&store, "R", "J", 1, 3).unwrap(),
            "first\nsecond\nthird\n",
            "the complete upload is authoritative"
        );

        delete_chunks(&store, "R", "J", 1);
        assert!(store.list("ci/logs/R/J/1/").unwrap().is_empty());
        assert_eq!(
            read_log(&store, "R", "J", 1, 3).unwrap(),
            "first\nsecond\nthird\n",
            "and the log survives its chunks"
        );

        // A prefix with nothing under it is a no-op rather than an error.
        delete_chunks(&store, "R", "nosuchjob", 1);
    }

    /// A store that is not answering at all is not the same as a log
    /// that is not there.
    ///
    /// `read_log` treats a 404 on the final object as "not uploaded yet"
    /// and falls through to the chunks, which is right — but only for a
    /// 404. Any other failure has to come back as an error, because
    /// falling through would answer a reader with the chunks it happens
    /// to have and no indication that the authoritative log exists and
    /// could not be read: a truncated log that looks complete. The
    /// deletion is the opposite case by design — it is best effort, so a
    /// store it cannot reach is a quiet no-op rather than something that
    /// could fail a runner's verdict.
    #[test]
    fn a_store_that_cannot_be_reached_fails_the_read_and_not_the_cleanup() {
        // Loopback, on a port nothing listens on: refused immediately,
        // and no test in this workspace reaches a network.
        let store = ObjectStore::new(
            "http://127.0.0.1:1/nobucket",
            stratum_store::LatencyModel::None,
        );
        let err = read_log(&store, "R", "J", 1, 3).expect_err("an unreachable store is an error");
        assert!(
            !err.contains("HTTP 404"),
            "and it is not mistaken for an absent log: {err}"
        );
        delete_chunks(&store, "R", "J", 1);
    }
}
