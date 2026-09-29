//! The daily license check.
//!
//! Wakes every `STRATUM_LICENSE_TICK_SECS` (an hour; 0 disables) and,
//! under one fleet-wide lock, checks the installed key with Weft's
//! license service when the last check is `STRATUM_LICENSE_CHECK_SECS`
//! (a day) old or there has been none. Asking the database rather than
//! counting ticks is what keeps a restart, or ten nodes, from checking
//! ten times a day. What the service answers is recorded for
//! `admin license-status` and logged; nothing the server does depends on
//! it (see [`crate::license`]).

use crate::app::SharedState;
use std::time::Duration;
use stratum_control::ids::now_ms;
use stratum_control::{jobs, ControlDb};

const WORKER: &str = "license-check";

pub fn spawn(state: SharedState) {
    let tick = super::env_secs("STRATUM_LICENSE_TICK_SECS", 3600);
    if tick == 0 {
        return;
    }
    let every_ms = super::env_secs("STRATUM_LICENSE_CHECK_SECS", 86_400).saturating_mul(1000);
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(tick));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let (db, cfg) = (state.db.clone(), state.license.clone());
            let out = tokio::task::spawn_blocking(move || {
                let Some(_held) = jobs::try_lock(&db, WORKER)? else {
                    return Ok(None);
                };
                if !due(&db, every_ms)? {
                    return Ok(None);
                }
                crate::license::run_check(&db, &cfg).map(Some)
            })
            .await;
            match out {
                Ok(Ok(Some(r))) => log(&r),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => eprintln!("weft: license check: {e}"),
                Err(e) => eprintln!("weft: license check: {e}"),
            }
        }
    });
}

/// A key is installed and its last check, if any, is `every_ms` old.
pub fn due(db: &ControlDb, every_ms: u64) -> Result<bool, String> {
    let Some(installed) = stratum_control::license::get(db)? else {
        return Ok(false);
    };
    let every = i64::try_from(every_ms).unwrap_or(i64::MAX);
    Ok(installed
        .checked_at
        .is_none_or(|at| now_ms().saturating_sub(at) >= every))
}

fn log(r: &crate::license::Report) {
    match (r.outcome, &r.status, &r.notice, &r.error) {
        ("answered", Some(status), Some(notice), _) => {
            eprintln!("weft: license: {status} — {notice}")
        }
        ("answered", Some(status), None, _) => eprintln!("weft: license: {status}"),
        (_, _, _, Some(e)) => eprintln!("weft: license check: {e}"),
        _ => {}
    }
}
