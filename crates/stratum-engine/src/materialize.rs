//! Materialize a layout into a local bare repo — the export path (R6) and
//! the compactor's seed. Exactly the clone a wire client would receive
//! (header + planned ranges + trailer), indexed by git, refs set from the
//! manifest, `fsck --full --strict` gated (I11 applies to us too).

use crate::gitcmd::{git, run, run_with_stdin};
use sha1::{Digest, Sha1};
use std::io::Read;
use std::path::Path;
use stratum_store::manifest::Manifest;
use stratum_store::ObjectStore;

/// Build `dest` (a fresh bare repo) from the layout at `prefix`.
/// Returns the manifest the materialization used.
pub fn materialize(store: &ObjectStore, prefix: &str, dest: &Path) -> Result<Manifest, String> {
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    materialize_manifest(store, &manifest, dest)?;
    Ok(manifest)
}

/// Materialize a *pinned* manifest snapshot — the compactor validates its
/// CAS against the etag it read alongside this manifest, so a concurrent
/// push forces a retry instead of being folded away silently.
pub fn materialize_manifest(
    store: &ObjectStore,
    manifest: &Manifest,
    dest: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    run(git(dest).args(["init", "-q", "--bare", "."]))?;

    let entries = manifest.total_entries();
    if entries > 0 {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(entries as u32).to_be_bytes());
        for part in manifest.clone_plan() {
            let mut r = store.get_stream(&part.key, part.range)?;
            let before = pack.len();
            r.read_to_end(&mut pack).map_err(|e| e.to_string())?;
            if (pack.len() - before) as u64 != part.expect_bytes {
                return Err(format!(
                    "{}: got {} bytes, expected {}",
                    part.key,
                    pack.len() - before,
                    part.expect_bytes
                ));
            }
        }
        let d = Sha1::digest(&pack);
        pack.extend_from_slice(&d);
        run_with_stdin(git(dest).args(["index-pack", "--stdin"]), &pack)
            .map_err(|e| format!("materialized stream rejected: {e}"))?;
    }

    let mut refs = manifest.refs.clone();
    for page in &manifest.ref_pages {
        refs.extend(stratum_store::refpages::load_page(store, page)?);
    }
    refs.sort();
    refs.dedup();
    for (name, oid) in &refs {
        run(git(dest).args(["update-ref", name, oid]))?;
    }
    if manifest.head.starts_with("refs/") {
        run(git(dest).args(["symbolic-ref", "HEAD", &manifest.head]))?;
    }
    // I11: never hand out an unverified materialization.
    run(git(dest).args(["fsck", "--full", "--strict"]))?;
    Ok(())
}

/// Export a layout as a git bundle at `bundle_path` (R6: standard-format
/// escape hatch — cloning the bundle needs nothing from Stratum).
pub fn export_bundle(
    store: &ObjectStore,
    prefix: &str,
    work_dir: &Path,
    bundle_path: &Path,
) -> Result<(), String> {
    let repo = work_dir.join("materialized.git");
    let manifest = materialize(store, prefix, &repo)?;
    if manifest.total_entries() == 0 {
        return Err("repo is empty; nothing to bundle".into());
    }
    run(git(&repo).args([
        "bundle",
        "create",
        bundle_path.to_str().ok_or("bad bundle path")?,
        "--all",
    ]))?;
    let _ = std::fs::remove_dir_all(&repo);
    Ok(())
}

/// Export a layout as a single **self-contained** packfile for CDN
/// offload (git's `packfile-uri`). Returns `(pack_hash, pack_path)`.
///
/// Built with `git pack-objects` rather than by reusing stored segments
/// on purpose: stored segments are entry *streams* whose delta bases
/// assume the whole concatenated clone stream, so they cannot be split
/// at an arbitrary boundary. `pack-objects` guarantees a pack with no
/// deltas pointing outside itself, which is what a client fetching this
/// pack in isolation requires.
pub fn export_pack(
    store: &ObjectStore,
    prefix: &str,
    work_dir: &Path,
) -> Result<(String, std::path::PathBuf), String> {
    let repo = work_dir.join("materialized.git");
    let _ = std::fs::remove_dir_all(&repo);
    let manifest = materialize(store, prefix, &repo)?;
    if manifest.total_entries() == 0 {
        return Err("repo is empty; nothing to pack".into());
    }
    let base = work_dir.join("cdn");
    // `--revs --all` (both command-line flags, as `git repack` invokes it)
    // packs every object reachable from any ref; pack-objects prints the
    // pack hash and writes <base>-<hash>.pack. Stdin stays empty: with
    // `--revs`, stdin carries *revision arguments*, and `--all` is a flag
    // rather than a rev (feeding it on stdin fails "not a rev '--all'").
    let out = run_with_stdin(
        git(&repo).args([
            "pack-objects",
            "--revs",
            "--all",
            base.to_str().ok_or("bad pack base path")?,
        ]),
        b"",
    )?;
    let hash = String::from_utf8_lossy(&out).trim().to_string();
    if hash.is_empty() {
        return Err("pack-objects produced no pack hash".into());
    }
    let path = work_dir.join(format!("cdn-{hash}.pack"));
    if !path.exists() {
        return Err(format!("pack-objects did not write {}", path.display()));
    }
    let _ = std::fs::remove_dir_all(&repo);
    Ok((hash, path))
}
