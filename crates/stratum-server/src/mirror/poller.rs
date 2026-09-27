//! Polling fallback (M1): every interval, mirrors whose last sync is older
//! than the interval get a sync. Doubles as webhook-loss recovery; the
//! sync itself is a cheap no-op when origin hasn't moved.

use crate::app::SharedState;
use std::time::Duration;
use stratum_control::registry;

pub fn spawn(state: SharedState, interval_secs: u64) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mirrors = match registry::list_mirrors(&state.db) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("weft: poller list failed: {e}");
                    continue;
                }
            };
            let cutoff = stratum_control::ids::now_ms() - (interval_secs as i64 * 1000);
            for repo in mirrors {
                if repo.last_sync_at.unwrap_or(0) > cutoff {
                    continue;
                }
                // Sequential on purpose: the poller is a floor, not a
                // firehose; webhooks carry the fast path.
                if let Err(e) = state.sync.sync(&repo).await {
                    eprintln!("weft: poll sync of {} failed: {e}", repo.id);
                }
            }
        }
    });
}
