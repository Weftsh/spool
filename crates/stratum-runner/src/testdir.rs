//! A scratch directory that removes itself, for the runner's own tests.
//!
//! Not `stratum_testkit::gitcli::Scratch`: this crate deliberately depends
//! on nothing but `ureq`, `serde_json` and `libc`, because the image it
//! ships in is the one that runs somebody else's shell commands and every
//! dependency in it is attack surface. A dev-dependency on the testkit
//! would drag Postgres and MinIO clients into that argument for the sake
//! of twenty lines.

use std::path::{Path, PathBuf};

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(hint: &str) -> TestDir {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("weft-runner-{hint}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create scratch dir");
        TestDir(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
