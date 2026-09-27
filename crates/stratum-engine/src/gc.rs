//! Epoch GC (port of the research `bench/gc_epochs.py`): an epoch
//! directory dies only when (a) no live pointer references it and (b) its
//! newest object is older than the grace window — which must exceed the
//! longest compaction and the longest clone, so a reader that loaded an
//! old manifest can finish streaming from it (I8's grace clause).
//!
//! Pointers are re-read immediately before deletion so a swap that lands
//! mid-sweep aborts the sweep instead of racing it.

use std::collections::HashSet;
use stratum_store::manifest::Manifest;
use stratum_store::ObjectStore;

/// Epochs of *this* layout that some **other** repository is reading.
///
/// A zero-copy fork does not copy bytes: it publishes an `SLH4` pointer
/// into upstream's data prefix and registers a reference. Liveness
/// therefore stops being a property of one prefix's own two pointers,
/// which is all `live_epochs` could ever see. Upstream compacts,
/// publishes a fresh epoch, the old one looks unreferenced, one grace
/// window passes, and every object under it is deleted — while forks are
/// still pointing at it.
///
/// The engine cannot answer this question itself; the answer lives in
/// the control plane. So it is a seam, implemented by the server against
/// `epoch_refs` and by `NoEpochRefs` where no fork can exist.
///
/// **Implementations must read fresh on every call.** `gc_epochs`
/// re-consults this immediately before deleting anything, and that
/// re-read is what closes the window where a fork is created mid-sweep.
/// A cached answer silently reopens it.
pub trait EpochRefs {
    /// Every epoch of the repository being swept that another
    /// repository depends on.
    ///
    /// Deliberately takes no argument. The engine identifies a layout by
    /// its store prefix and the control plane identifies one by its repo
    /// id, and a parameter here would be an invitation to look the
    /// answer up under the wrong key. A resolver is constructed for the
    /// one repository it answers about, so the binding is made once, by
    /// the caller that has both identities in hand.
    ///
    /// An `Err` **aborts the sweep**. That is the only safe reading of
    /// "I could not find out what is referenced": deleting on an
    /// incomplete answer is unrecoverable, and not deleting costs
    /// storage until the next pass.
    fn pinned_epochs(&self) -> Result<HashSet<String>, String>;
}

/// The correct resolver for a store where no repository can reference
/// another's data — and a data-loss bug anywhere else. Named rather than
/// defaulted so that using it is a decision somebody made.
pub struct NoEpochRefs;

impl EpochRefs for NoEpochRefs {
    fn pinned_epochs(&self) -> Result<HashSet<String>, String> {
        Ok(HashSet::new())
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct GcReport {
    pub epochs_seen: usize,
    pub epochs_deleted: usize,
    pub objects_deleted: usize,
}

/// Epochs the current pointers (manifest + locator.hdr) reference: every
/// absolute key's epoch segment plus the hdr's own epoch field.
fn live_epochs(
    store: &ObjectStore,
    prefix: &str,
    refs: &dyn EpochRefs,
) -> Result<HashSet<String>, String> {
    let mut live = HashSet::new();
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    let mut keys: Vec<&str> = Vec::new();
    keys.extend(manifest.segments.iter().map(|s| s.key.as_str()));
    keys.extend(manifest.cold_segments.iter().map(|s| s.key.as_str()));
    keys.extend(manifest.hot_segments.iter().map(|s| s.key.as_str()));
    if let Some(s) = &manifest.snapshot {
        keys.push(&s.key);
    }
    if let Some(l) = &manifest.locator {
        keys.push(&l.key);
        keys.push(&l.chains_key);
    }
    keys.extend(manifest.ref_pages.iter().map(|p| p.key.as_str()));
    for w in &manifest.wal {
        keys.push(&w.key);
        keys.push(&w.oids_key);
    }
    let key_prefix = format!("{prefix}/");
    for k in keys {
        if let Some(rest) = k.strip_prefix(&key_prefix) {
            if let Some((epoch, _)) = rest.split_once('/') {
                live.insert(epoch.to_string());
            }
        }
    }
    live.insert(manifest.epoch.clone());
    // The locator.hdr pointer may lag the manifest (I15); its epoch is
    // live in its own right.
    //
    // An *absent* header is normal — a repo whose plane has not been
    // built yet — and is tolerated. An unparseable one is not: sweeping
    // on a live set we know to be incomplete is how you delete data a
    // pointer still references, so a corrupt pointer stops the sweep
    // instead of narrowing it. formats.md puts it as "a reader that sees
    // an unknown magic must fail loudly, never guess".
    if let Ok(hdr) = store.get(&format!("{prefix}/locator.hdr")) {
        let parsed = stratum_store::plane::parse_header(&hdr)
            .map_err(|e| format!("{prefix}/locator.hdr: {e}"))?;
        // SLH4 points at an absolute data prefix, which for a fork is
        // another repository's. That epoch is not ours to keep alive and
        // not ours to sweep: its liveness is the `epoch_refs` reference
        // the fork registered against upstream. Only count the epoch
        // when the bytes it names are actually under our own prefix.
        let local = match &parsed.data_prefix {
            None => true,
            Some(p) => p
                .strip_prefix(&format!("{prefix}/"))
                .is_some_and(|rest| !rest.contains('/')),
        };
        if local {
            live.insert(parsed.epoch);
        }
    }
    // Whatever another repository is reading is live here, whether or
    // not either of our own pointers has heard of it.
    live.extend(refs.pinned_epochs()?);
    Ok(live)
}

/// Sweep one repo's layout. `now_secs` is passed in for testability.
/// Sweep one repo's layout, honouring references held by other repos.
///
/// `now_secs` is passed in for testability.
pub fn gc_epochs_with_refs(
    store: &ObjectStore,
    prefix: &str,
    grace_secs: u64,
    now_secs: u64,
    refs: &dyn EpochRefs,
) -> Result<GcReport, String> {
    // A layout with no manifest has no pointers, and a sweep decides what
    // to delete *from* the pointers. Reading that as an error made every
    // GC tick log a failure for every repository whose storage does not
    // exist yet — which a fork's does not until its job publishes one,
    // since a fork deliberately skips the empty-repo init that would
    // otherwise collide with the manifest it is about to inherit.
    //
    // Skipped rather than swept with an empty live set, and the
    // difference matters: an empty live set says "nothing is referenced,
    // take it all", which is the correct reading of a repository with no
    // pointers *and* the catastrophic reading of one whose manifest is
    // merely unreachable. There is nothing to gain by guessing between
    // them, so a layout with no manifest is left alone.
    match store.get(&format!("{prefix}/manifest.json")) {
        Ok(_) => {}
        Err(e) if e.contains("HTTP 404") => return Ok(GcReport::default()),
        Err(e) => return Err(e),
    }
    let live = live_epochs(store, prefix, refs)?;
    let listing = store.list(&format!("{prefix}/"))?;
    // Group keys by epoch dir; skip the pointers and non-epoch dirs.
    let mut by_epoch: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let key_prefix = format!("{prefix}/");
    for (key, last_modified) in listing {
        let Some(rest) = key.strip_prefix(&key_prefix) else {
            continue;
        };
        let Some((epoch, _)) = rest.split_once('/') else {
            continue; // manifest.json / locator.hdr
        };
        if epoch == "exports" || epoch == "audit" {
            continue;
        }
        by_epoch
            .entry(epoch.to_string())
            .or_default()
            .push((key, last_modified));
    }

    let mut report = GcReport {
        epochs_seen: by_epoch.len(),
        ..Default::default()
    };
    for (epoch, keys) in by_epoch {
        if live.contains(&epoch) {
            continue;
        }
        let newest = keys
            .iter()
            .filter_map(|(_, lm)| parse_rfc3339_secs(lm))
            .max()
            .unwrap_or(u64::MAX);
        if now_secs.saturating_sub(newest) < grace_secs {
            continue;
        }
        // Re-read pointers right before the sweep: a swap that landed
        // mid-scan must abort, not race. With forks this re-read carries
        // a second job — a fork created since the scan began has
        // registered its reference by now, and this is where we find
        // out. It is the whole reason the fork worker must register the
        // reference *before* publishing its pointer.
        let live_now = live_epochs(store, prefix, refs)?;
        if live_now.contains(&epoch) {
            continue;
        }
        for (key, _) in &keys {
            store.delete(key)?;
            report.objects_deleted += 1;
        }
        report.epochs_deleted += 1;
    }
    Ok(report)
}

/// Delete everything under a prefix (deleted-repo sweep). Returns the
/// object count removed.
pub fn sweep_prefix(store: &ObjectStore, prefix: &str) -> Result<usize, String> {
    let listing = store.list(&format!("{prefix}/"))?;
    let n = listing.len();
    for (key, _) in listing {
        store.delete(&key)?;
    }
    Ok(n)
}

/// "2026-08-20T12:34:56.000Z" (S3 LastModified) → unix seconds.
pub fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u64> {
        std::str::from_utf8(&b[r]).ok()?.parse().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days-from-civil (Hinnant).
    let y = y as i64 - if mo <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parse() {
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_rfc3339_secs("2026-08-19T23:59:07.123Z"),
            Some(1_787_183_947)
        );
        assert_eq!(parse_rfc3339_secs("garbage"), None);
    }
}
