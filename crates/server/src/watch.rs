//! Listeners: observe properties of Live objects and report what changed.
//!
//! Subscriptions live in the Remote Script, per connection. When the connection to Live is
//! replaced (Live restarted, or the control surface was re-selected), they are gone; the next
//! call reports them as lost instead of silently watching nothing.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use live_mcp_bridge::{Bridge, BridgeError};
use live_mcp_protocol::{Event, Target};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::live::Live;
use crate::mcp::MAX_RESULT_CHARS;

/// At most this many events are returned per call; the oldest beyond it are counted, not listed.
const MAX_EVENTS: usize = 500;
/// Room kept free in a response for the fields added after the events are chosen (`omitted`, `note`).
const RESERVED_CHARS: usize = 1024;
/// Longest `wait_ms`.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// How often `events` asks the script while waiting. The script answers on its ~10 Hz ticks.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize)]
struct Watch {
    subscription: u64,
    target: String,
    prop: String,
}

#[derive(Default)]
struct State {
    /// The connection the subscriptions were made on.
    bridge: Weak<Bridge>,
    subs: BTreeMap<u64, Watch>,
    /// Subscriptions that ended with an earlier connection, not yet reported.
    lost: Vec<Watch>,
}

#[derive(Default)]
pub struct Watches {
    state: Mutex<State>,
}

impl Watches {
    /// Records that `bridge` is the current connection. If it is not the one the
    /// subscriptions were made on, they ended with the old connection: move them to `lost`.
    fn sync(&self, bridge: &Arc<Bridge>) {
        let mut state = self.state.lock().expect("watch state");
        if !Weak::ptr_eq(&state.bridge, &Arc::downgrade(bridge)) {
            let ended: Vec<Watch> = std::mem::take(&mut state.subs).into_values().collect();
            state.lost.extend(ended);
            state.bridge = Arc::downgrade(bridge);
        }
    }

    /// Records a subscription made on `bridge`. If that connection was replaced while the
    /// subscription was being made, it ended with it: report it as lost instead.
    fn record(&self, bridge: &Arc<Bridge>, watch: Watch) {
        let mut state = self.state.lock().expect("watch state");
        if Weak::ptr_eq(&state.bridge, &Arc::downgrade(bridge)) {
            // Keep the first label an existing subscription was given.
            state.subs.entry(watch.subscription).or_insert(watch);
        } else {
            state.lost.push(watch);
        }
    }

    fn has_lost(&self) -> bool {
        !self.state.lock().expect("watch state").lost.is_empty()
    }

    /// Takes the loss notices for a response. Call it only once nothing can fail before the
    /// response goes out, or the only copy of a notice is gone.
    fn take_lost(&self) -> Vec<Watch> {
        std::mem::take(&mut self.state.lock().expect("watch state").lost)
    }

    fn watching(&self) -> Vec<Watch> {
        self.state.lock().expect("watch state").subs.values().cloned().collect()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ObserveParams {
    /// The Live object: a path from live_set/live_app ("live_set tracks 0"), "#ID", or "#ID relative path".
    pub target: String,
    /// One or more of its observable properties (`describe` lists them), e.g. ["tempo", "is_playing"].
    pub props: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EventsParams {
    /// If nothing has changed yet, wait up to this long (milliseconds, at most 60000) for the first change.
    #[serde(default)]
    pub wait_ms: u64,
    /// Only the latest change of each subscription, with a count of how many changes there were.
    #[serde(default)]
    pub latest_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnobserveParams {
    /// Subscription ids to remove. Leave out to remove all.
    pub subscriptions: Option<Vec<u64>>,
}

pub async fn observe(live: &Live, watches: &Watches, p: ObserveParams) -> Result<Value> {
    if p.props.is_empty() {
        bail!("props is empty; name at least one observable property (describe lists them)");
    }
    let target: Target = p.target.parse().map_err(anyhow::Error::msg)?;
    let bridge = live.bridge().await?;
    watches.sync(&bridge);
    let mut watching = Vec::new();
    let mut errors = Vec::new();
    for prop in &p.props {
        match bridge.observe(&target, prop).await {
            Ok(result) => {
                let Some(subscription) = result["subscription"].as_u64() else {
                    bail!("unexpected observe response: {result}");
                };
                let watch = Watch { subscription, target: p.target.trim().to_owned(), prop: prop.clone() };
                let mut entry = Map::new();
                entry.insert("subscription".into(), json!(subscription));
                entry.insert("prop".into(), json!(prop));
                if let Some(value) = result.get("value") {
                    entry.insert("value".into(), value.clone());
                }
                if result["existing"] == true {
                    entry.insert("existing".into(), json!(true));
                }
                watches.record(&bridge, watch);
                watching.push(Value::Object(entry));
            }
            Err(e @ BridgeError::Timeout { .. }) => errors.push(json!({
                "prop": prop,
                "error": format!(
                    "{e}. If Live applied it, its changes are reported without a label; observe it again \
                     to get its subscription (the existing one is returned)"
                ),
            })),
            Err(e) => errors.push(json!({"prop": prop, "error": e.to_string()})),
        }
    }
    if watching.is_empty() {
        let messages: Vec<&str> = errors.iter().filter_map(|e| e["error"].as_str()).collect();
        bail!("{}", messages.join("; "));
    }
    let mut out = json!({"target": p.target.trim(), "watching": watching});
    if !errors.is_empty() {
        out["errors"] = json!(errors);
    }
    let lost = watches.take_lost();
    if !lost.is_empty() {
        out["lost"] = lost_report(&lost);
    }
    Ok(out)
}

pub async fn events(live: &Live, watches: &Watches, p: EventsParams) -> Result<Value> {
    let wait = Duration::from_millis(p.wait_ms).min(MAX_WAIT);
    let deadline = Instant::now() + wait;
    let bridge = live.bridge().await?;
    watches.sync(&bridge);

    let mut events = Vec::new();
    let mut dropped = 0;
    // Waiting for a change makes no sense when the observations just ended.
    let lost_pending = watches.has_lost();
    loop {
        let listening = match bridge.events().await {
            Ok(batch) => {
                events.extend(batch.events);
                dropped += batch.dropped;
                batch.listening
            }
            Err(BridgeError::Closed) => {
                // Live went away (e.g. while we waited): the subscriptions went with it.
                let current = live.bridge().await?;
                watches.sync(&current);
                break;
            }
            Err(e) => return Err(e.into()),
        };
        // Nor when no listener is left that could fire: nothing is observed, or only deleted objects.
        if !events.is_empty() || lost_pending || listening == 0 || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }

    let watching = watches.watching();
    let labels: HashMap<u64, &Watch> = watching.iter().map(|w| (w.subscription, w)).collect();
    let now = unix_now();
    let reported: Vec<(Value, u64)> = if p.latest_only {
        // One entry per subscription, at its latest change, in the order of those changes.
        let mut changes: HashMap<u64, u64> = HashMap::new();
        let mut last: HashMap<u64, usize> = HashMap::new();
        for (i, event) in events.iter().enumerate() {
            *changes.entry(event.subscription).or_default() += 1;
            last.insert(event.subscription, i);
        }
        events
            .iter()
            .enumerate()
            .filter(|(i, e)| last[&e.subscription] == *i)
            .map(|(_, e)| (describe_event(e, labels.get(&e.subscription).copied(), now), changes[&e.subscription]))
            .collect()
    } else {
        events.iter().map(|e| (describe_event(e, labels.get(&e.subscription).copied(), now), 1)).collect()
    };
    let entries: Vec<Value> = reported
        .into_iter()
        .map(|(mut event, changes)| {
            if p.latest_only {
                event["changes"] = json!(changes);
            }
            event
        })
        .collect();

    // Nothing below can fail, so the loss notices are safe to take now.
    let lost = watches.take_lost();
    let nothing_observed = labels.is_empty();
    let mut out = json!({"events": [], "watching": watching});
    if dropped > 0 {
        out["dropped"] = json!(dropped);
    }
    if !lost.is_empty() {
        out["lost"] = lost_report(&lost);
    }
    // The events have already left the script's queue, so the response must not be too large to
    // send: list the latest that fit, rather than failing and losing them all.
    let budget = MAX_RESULT_CHARS.saturating_sub(json_len(&out) + RESERVED_CHARS);
    let fitted = fit(entries, MAX_EVENTS, budget);
    out["events"] = json!(fitted.events);
    if fitted.omitted > 0 {
        out["omitted"] = json!(fitted.omitted);
        out["note"] = json!(if p.latest_only {
            "only the latest changes fit in one response; read the other observed properties with get".to_owned()
        } else {
            format!(
                "only the latest changes fit in one response (at most {MAX_EVENTS}, within the size limit); \
                 use latest_only to see one entry per subscription"
            )
        });
    } else if fitted.elided > 0 {
        out["note"] = json!("some values are too large to list; read those properties with get");
    } else if lost.is_empty() && nothing_observed {
        out["note"] = json!("nothing is observed; use observe first");
    }
    Ok(out)
}

pub async fn unobserve(live: &Live, watches: &Watches, p: UnobserveParams) -> Result<Value> {
    let bridge = live.bridge().await?;
    watches.sync(&bridge);
    let result = bridge.unobserve(p.subscriptions.as_deref()).await?;
    let removed: Vec<u64> = result["removed"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    let mut state = watches.state.lock().expect("watch state");
    let removed: Vec<Watch> = removed.iter().filter_map(|id| state.subs.remove(id)).collect();
    // The script drops any unread events of removed subscriptions, so nothing is reported for them later.
    let mut out = json!({"removed": removed, "watching": state.subs.values().cloned().collect::<Vec<_>>()});
    if result["unknown"].as_array().is_some_and(|u| !u.is_empty()) {
        out["unknown"] = result["unknown"].clone();
    }
    drop(state);
    let lost = watches.take_lost();
    if !lost.is_empty() {
        out["lost"] = lost_report(&lost);
    }
    Ok(out)
}

fn describe_event(event: &Event, watch: Option<&Watch>, now: f64) -> Value {
    let mut out = Map::new();
    out.insert("subscription".into(), json!(event.subscription));
    if let Some(watch) = watch {
        out.insert("target".into(), json!(watch.target));
        out.insert("prop".into(), json!(watch.prop));
    }
    if let Some(value) = &event.value {
        out.insert("value".into(), value.clone());
    }
    out.insert("ago_ms".into(), json!(((now - event.time) * 1000.0).max(0.0).round() as u64));
    Value::Object(out)
}

struct Fitted {
    /// Oldest first.
    events: Vec<Value>,
    /// The oldest entries, left out.
    omitted: usize,
    /// Entries listed without their value, because it alone would not fit.
    elided: usize,
}

/// Keeps the latest of `entries` (oldest first) that fit in `budget` characters of JSON, and at
/// most `max` of them. An entry too large for the room left is kept without its value, if
/// that fits, so one large value does not crowd out every older entry.
fn fit(entries: Vec<Value>, max: usize, budget: usize) -> Fitted {
    let total = entries.len();
    let mut kept = Vec::new();
    let mut used = 2; // the brackets around the list
    let mut elided = 0;
    for mut entry in entries.into_iter().rev() {
        if kept.len() == max {
            break;
        }
        let mut size = json_len(&entry) + 1; // and a comma
        let mut without_value = false;
        if used + size > budget
            && let Some(obj) = entry.as_object_mut()
            && let Some(value) = obj.remove("value")
        {
            let chars = json_len(&value);
            obj.insert(
                "value_omitted".into(),
                json!(format!("too large to list ({chars} characters); read the property with get")),
            );
            size = json_len(&entry) + 1;
            without_value = true;
        }
        if used + size > budget {
            break;
        }
        used += size;
        elided += usize::from(without_value);
        kept.push(entry);
    }
    kept.reverse();
    Fitted { omitted: total - kept.len(), events: kept, elided }
}

fn json_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |s| s.len())
}

fn lost_report(lost: &[Watch]) -> Value {
    json!({
        "subscriptions": lost,
        "reason": "the connection to Live was reset (Live restarted, or the control surface was re-selected); \
                   observe them again",
    })
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(subscription: u64, value: Value) -> Value {
        json!({"subscription": subscription, "value": value, "ago_ms": 0})
    }

    #[test]
    fn fit_keeps_the_latest_that_fit() {
        let entries: Vec<Value> = (0..10).map(|i| entry(i, json!("x".repeat(100)))).collect();
        let one = json_len(&entries[0]) + 1;
        let fitted = fit(entries.clone(), 500, 2 + 3 * one);
        assert_eq!((fitted.omitted, fitted.elided), (7, 0));
        assert_eq!(fitted.events, entries[7..]);
        assert!(json_len(&json!(fitted.events)) <= 2 + 3 * one);

        let fitted = fit(entries.clone(), 4, usize::MAX);
        assert_eq!((fitted.omitted, fitted.events), (6, entries[6..].to_vec()));
        let fitted = fit(entries.clone(), 500, usize::MAX);
        assert_eq!((fitted.omitted, fitted.events), (0, entries));
    }

    #[test]
    fn fit_lists_a_value_too_large_on_its_own_without_it() {
        let entries = vec![entry(1, json!(1)), entry(2, json!("x".repeat(5000))), entry(3, json!(3))];
        let fitted = fit(entries.clone(), 500, 1000);
        assert_eq!((fitted.omitted, fitted.elided), (0, 1));
        assert_eq!(fitted.events[0], entries[0]);
        assert_eq!(fitted.events[2], entries[2]);
        let big = &fitted.events[1];
        assert_eq!(big["subscription"], 2);
        assert!(big.get("value").is_none());
        assert!(big["value_omitted"].as_str().unwrap().contains("5002 characters"), "{big}");
        assert!(json_len(&json!(fitted.events)) <= 1000);
    }

    #[test]
    fn fit_lists_a_value_too_large_for_the_room_left_without_it() {
        // Each value fits on its own, but not both: the older is listed without its value,
        // and the small entries before it are still listed.
        let small = entry(1, json!(1));
        let budget = 2 + 2 * (json_len(&small) + 1) + 3000 + 200;
        let entries = vec![
            small.clone(),
            entry(2, json!("x".repeat(2000))),
            entry(3, json!("y".repeat(2000))),
            entry(4, json!(4)),
        ];
        let fitted = fit(entries.clone(), 500, budget);
        assert_eq!((fitted.omitted, fitted.elided), (0, 1), "{:?}", fitted.events);
        assert_eq!(fitted.events[0], small);
        assert!(fitted.events[1].get("value").is_none() && fitted.events[1].get("value_omitted").is_some());
        assert_eq!(fitted.events[2..], entries[2..]);
        assert!(json_len(&json!(fitted.events)) <= budget);
    }

    #[test]
    fn fit_with_no_room_lists_nothing() {
        let fitted = fit(vec![entry(1, json!(1))], 500, 0);
        assert_eq!((fitted.events.len(), fitted.omitted, fitted.elided), (0, 1, 0));
    }
}
