//! N stateless servers over one bucket and one control plane.
//!
//! This is the production topology. `README.md` claims any stateless node
//! serves any repo, and every invariant about pointers and epochs — I7,
//! I8, I9, I15 — is written for readers and writers that never talk to
//! each other and coordinate only through the object store's CAS. Until
//! this harness existed the only suite that spawned two servers tested a
//! *database* fence; nothing exercised the object-store commit point from
//! more than one process at a time.
//!
//! Three details here are load-bearing, and each one has a reason:
//!
//! * **Nodes start one at a time.** `spawn_on_free_port` documents why
//!   picking a port by binding it and letting it go is racy, and why the
//!   losing side of that race fails as "the API forgot my org" rather
//!   than as a port clash. Starting a fleet in parallel would re-open
//!   exactly that window, several times over, in every fleet test.
//!
//! * **Each node gets its own `STRATUM_DATA_DIR`.** The compaction and
//!   quarantine scratch is node-local by design — in production these are
//!   separate containers with separate disks. Pointing two nodes at one
//!   directory would produce collisions that look like a product bug and
//!   are not one.
//!
//! * **Every background poller is off by default.** A test that wants
//!   compaction asks for it, on the node it wants, at the moment it
//!   wants; a sweep firing on its own turns "did the swap happen under
//!   the reader" into a coin flip.

use crate::gitcli::{self, Scratch};
use crate::server::{Server, ServerBuilder};
use std::collections::BTreeMap;
use stratum_store::{LatencyModel, Manifest, ObjectStore};

/// Pollers that would otherwise fire on their own schedule. Tests opt in
/// per node by overriding these.
const QUIET: [(&str, &str); 6] = [
    ("STRATUM_COMPACT_POLL_SECS", "0"),
    ("STRATUM_CDNPACK_POLL_SECS", "0"),
    ("STRATUM_LAND_POLL_SECS", "0"),
    ("STRATUM_GC_SECS", "0"),
    ("STRATUM_USAGE_ROLLUP_SECS", "0"),
    ("STRATUM_AUDIT_SHIP_SECS", "0"),
];

pub struct Fleet {
    nodes: Vec<Server>,
    /// The bucket every node writes to — the real S3 URL, not a proxy.
    pub bucket_url: String,
    pub db_url: String,
    pub org: String,
    /// The org admin token, minted once on node 0 and valid everywhere
    /// because the control plane is shared.
    pub admin: String,
    scratch: Scratch,
}

pub struct FleetBuilder {
    bin: String,
    bucket_url: String,
    node_store_urls: BTreeMap<usize, String>,
    nodes: usize,
    hint: String,
    org: String,
    env: Vec<(String, String)>,
}

impl Fleet {
    /// `bucket_url` is the shared store every node commits into.
    pub fn builder(bin: &str, bucket_url: &str) -> FleetBuilder {
        FleetBuilder {
            bin: bin.to_string(),
            bucket_url: bucket_url.to_string(),
            node_store_urls: BTreeMap::new(),
            nodes: 2,
            hint: "fleet".to_string(),
            org: "acme".to_string(),
            env: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, i: usize) -> &Server {
        &self.nodes[i]
    }

    pub fn nodes(&self) -> &[Server] {
        &self.nodes
    }

    /// A scratch directory the test owns for the fleet's lifetime.
    pub fn scratch(&self) -> &std::path::Path {
        self.scratch.path()
    }

    /// A store handle pointed at the shared bucket directly, for
    /// assertions that must see what actually landed rather than what a
    /// node says landed.
    pub fn store(&self) -> ObjectStore {
        ObjectStore::new(&self.bucket_url, LatencyModel::None)
    }

    pub fn manifest(&self, prefix: &str) -> Manifest {
        let bytes = self
            .store()
            .get(&format!("{prefix}/manifest.json"))
            .unwrap_or_else(|e| panic!("read {prefix}/manifest.json: {e}"));
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse manifest: {e}"))
    }

    /// Create a repo through node 0 and return its store prefix
    /// (`o/<org>/r/<repo>/prod`).
    pub fn create_repo(&self, name: &str) -> String {
        let (st, out) = self.nodes[0].post(
            &format!("/v1/orgs/{}/repos", self.org),
            &self.admin,
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 201, "create repo {name}: {out}");
        format!(
            "o/{}/r/{}/prod",
            out["org_id"].as_str().expect("org_id"),
            out["id"].as_str().expect("repo id")
        )
    }

    /// The REST path prefix for a repo, on any node.
    pub fn repo_path(&self, repo: &str) -> String {
        format!("/v1/orgs/{}/repos/{repo}", self.org)
    }

    /// A git remote against node `i`, with the admin token embedded.
    pub fn url(&self, i: usize, repo: &str) -> String {
        self.nodes[i].authed_url(&self.admin, &self.org, repo)
    }

    /// What a real client sees from node `i`: `git ls-remote` over the
    /// wire, not the REST view. Refs converging is a claim about the
    /// protocol surface, so it is asserted there.
    pub fn refs_from(&self, i: usize, repo: &str) -> BTreeMap<String, String> {
        let out = gitcli::git(self.scratch.path(), &["ls-remote", &self.url(i, repo)]);
        out.lines()
            .filter_map(|l| {
                let (oid, name) = l.split_once('\t')?;
                Some((name.to_string(), oid.to_string()))
            })
            .collect()
    }

    /// Every node must advertise exactly the same refs. Panics naming the
    /// divergent pair and the refs that differ — "the fleet disagrees" is
    /// useless during an incident, "node 0 and node 3 disagree about
    /// refs/heads/b2" is not.
    pub fn assert_refs_converged(&self, repo: &str) {
        let all: Vec<BTreeMap<String, String>> =
            (0..self.len()).map(|i| self.refs_from(i, repo)).collect();
        for j in 1..all.len() {
            if all[0] != all[j] {
                let differing: Vec<String> = all[0]
                    .keys()
                    .chain(all[j].keys())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .filter(|k| all[0].get(*k) != all[j].get(*k))
                    .map(|k| {
                        format!(
                            "  {k}: node 0 = {:?}, node {j} = {:?}",
                            all[0].get(k),
                            all[j].get(k)
                        )
                    })
                    .collect();
                panic!(
                    "node 0 and node {j} disagree about {repo}:\n{}",
                    differing.join("\n")
                );
            }
        }
    }

    /// Clone `repo` from node `i` into `dest` and run the I11 gate.
    /// Returns the clone's HEAD commit.
    pub fn clone_and_fsck_from(&self, i: usize, repo: &str, dest: &std::path::Path) -> String {
        gitcli::clone_and_fsck(&self.url(i, repo), dest)
    }
}

impl FleetBuilder {
    pub fn nodes(mut self, n: usize) -> Self {
        self.nodes = n;
        self
    }

    /// Names the minted database and the scratch directory.
    pub fn hint(mut self, hint: &str) -> Self {
        self.hint = hint.to_string();
        self
    }

    pub fn org(mut self, org: &str) -> Self {
        self.org = org.to_string();
        self
    }

    /// Applied to every node.
    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_string(), value.into()));
        self
    }

    /// Send node `i`'s store traffic somewhere other than the bucket —
    /// a proxy, usually. The bucket is still the same bucket; only this
    /// node's route to it changes, so a test can watch one node's keys
    /// without the others' traffic in the way.
    pub fn node_store_url(mut self, i: usize, url: &str) -> Self {
        self.node_store_urls.insert(i, url.to_string());
        self
    }

    pub fn start(self) -> Fleet {
        assert!(self.nodes > 0, "a fleet needs at least one node");
        let scratch = Scratch::new(&self.hint);
        let db_url = crate::pg::test_db_url(&self.hint);
        let mut nodes = Vec::with_capacity(self.nodes);
        // Sequential, deliberately: see the module comment on the port race.
        for i in 0..self.nodes {
            let store_url = self
                .node_store_urls
                .get(&i)
                .cloned()
                .unwrap_or_else(|| self.bucket_url.clone());
            let mut b: ServerBuilder = Server::builder(&self.bin, &store_url)
                .db_url(&db_url)
                .data_dir(scratch.path().join(format!("node-{i}")));
            for (k, v) in QUIET {
                b = b.env(k, v);
            }
            for (k, v) in &self.env {
                b = b.env(k, v.clone());
            }
            nodes.push(b.start());
        }
        // One bootstrap, on node 0: the control plane is shared, so the
        // token it mints authenticates against every node.
        let admin = nodes[0].bootstrap_org(&self.org);
        Fleet {
            nodes,
            bucket_url: self.bucket_url,
            db_url,
            org: self.org,
            admin,
            scratch,
        }
    }
}
