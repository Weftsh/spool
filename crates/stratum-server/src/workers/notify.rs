//! Repos push webhooks: HMAC-signed deliveries with bounded retries.

use crate::app::SharedState;
use hmac::Mac;
use std::time::Duration;
use stratum_control::webhooks;

/// Fire-and-forget notification to every subscription on the repo.
pub fn notify(state: &SharedState, repo_id: &str, event: &str, payload: serde_json::Value) {
    let subs = match webhooks::for_repo(&state.db, repo_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("weft: webhook lookup: {e}");
            return;
        }
    };
    for sub in subs {
        let db = state.db.clone();
        let event = event.to_string();
        let body = serde_json::json!({
            "event": event,
            "repo_id": repo_id,
            "payload": payload,
        })
        .to_string();
        tokio::spawn(async move {
            let mut last_err = String::new();
            for attempt in 1..=3u32 {
                let sub2 = sub.clone();
                let body2 = body.clone();
                let sent = tokio::task::spawn_blocking(move || deliver(&sub2, &body2))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")));
                match sent {
                    Ok(()) => {
                        let _ = webhooks::record_delivery(
                            &db,
                            &sub.id,
                            &event,
                            "delivered",
                            attempt as i64,
                            None,
                        );
                        return;
                    }
                    Err(e) => last_err = e,
                }
                tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
            }
            let _ = webhooks::record_delivery(&db, &sub.id, &event, "failed", 3, Some(&last_err));
        });
    }
}

fn deliver(sub: &webhooks::Subscription, body: &str) -> Result<(), String> {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(sub.secret.as_bytes())
        .map_err(|e| e.to_string())?;
    mac.update(body.as_bytes());
    let sig = stratum_store::pack::hex(&mac.finalize().into_bytes());
    ureq::post(&sub.url)
        .set("Content-Type", "application/json")
        .set("X-Weft-Signature-256", &format!("sha256={sig}"))
        .timeout(Duration::from_secs(10))
        .send_string(body)
        .map(|_| ())
        .map_err(|e| e.to_string())
}
