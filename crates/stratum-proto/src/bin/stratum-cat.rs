//! Single-object read CLI over the locator plane (experiment 005, H2).
//! Thin wrapper: the actual plane/lookup/resolution logic lives in
//! stratum_store::plane so the write path shares it.
//!
//! Usage: stratum-cat <repo> <oid-hex> [--quiet]
//! Env: STRATUM_STORE_URL, STRATUM_LATENCY_MODEL, STRATUM_LAYOUT,
//!      STRATUM_LOCATOR_TIER=same|kv (hdr/bucket/chain reads at KV latency)
//!
//! Prints a JSON line with internal timing and GET count; object bytes are
//! verified against the oid (correctness gate) and optionally printed.

use sha1::{Digest, Sha1};
use std::io::Write;
use std::time::Instant;
use stratum_store::pack::{hex, type_name};
use stratum_store::{LatencyModel, ObjectStore, Plane};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args[2].len() != 40 || !args[2].bytes().all(|b| b.is_ascii_hexdigit()) {
        die("usage: stratum-cat <repo> <40-hex-oid> [--quiet]");
    }
    let (repo, oid) = (&args[1], args[2].to_lowercase());
    let quiet = args.iter().any(|a| a == "--quiet");
    let layout = std::env::var("STRATUM_LAYOUT").unwrap_or_else(|_| "tiered-1024".into());

    let t0 = Instant::now();
    let store = ObjectStore::from_env();
    let base = std::env::var("STRATUM_STORE_URL").unwrap();
    let locator_store = if std::env::var("STRATUM_LOCATOR_TIER").as_deref() == Ok("kv")
        && LatencyModel::from_env() != LatencyModel::None
    {
        ObjectStore::new(&base, LatencyModel::Kv)
    } else {
        ObjectStore::new(&base, LatencyModel::from_env())
    };

    let prefix = format!("{repo}/{layout}");
    let plane = Plane::load(&locator_store, &prefix).unwrap_or_else(|e| die(&e));
    let (typ, data, gets) = plane
        .read_object(&store, &locator_store, &oid)
        .unwrap_or_else(|e| die(&e));

    // Correctness gate: the loose-object header + payload must hash to oid.
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", type_name(typ), data.len()).as_bytes());
    h.update(&data);
    let got = hex(&h.finalize());
    if got != oid {
        die(&format!("sha mismatch: {got} != {oid}"));
    }

    let elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    println!(
        "{{\"oid\":\"{oid}\",\"type\":\"{}\",\"size\":{},\"ms\":{:.2},\"gets\":{}}}",
        type_name(typ),
        data.len(),
        elapsed_ms,
        gets + 1 // + locator.hdr GET
    );
    if !quiet {
        std::io::stdout().write_all(&data).ok();
    }
}

fn die(msg: &str) -> ! {
    eprintln!("stratum-cat: {msg}");
    std::process::exit(1)
}
