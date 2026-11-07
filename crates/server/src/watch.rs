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

/// At most this many events are returned per call; the oldest beyond it are counted, not listed.
const MAX_EVENTS: usize = 500;
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
                // Keep the first label an existing subscription was given.
                watches.state.lock().expect("watch state").subs.entry(subscription).or_insert(watch);
                watching.push(Value::Object(entry));
            }
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
    let mut lost = watches.take_lost();

    let mut events = Vec::new();
    let mut dropped = 0;
    // Waiting for a change makes no sense when nothing is observed (or the observations just ended).
    let waitable = lost.is_empty() && !watches.watching().is_empty();
    loop {
        match bridge.events().await {
            Ok(batch) => {
                events.extend(batch.events);
                dropped += batch.dropped;
            }
            Err(BridgeError::Closed) => {
                // Live went away (e.g. while we waited): the subscriptions went with it.
                let current = live.bridge().await?;
                watches.sync(&current);
                lost.extend(watches.take_lost());
                break;
            }
            Err(e) => return Err(e.into()),
        }
        if !events.is_empty() || !waitable || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }

    let labels: HashMap<u64, Watch> = watches.watching().into_iter().map(|w| (w.subscription, w)).collect();
    let now = unix_now();
    let mut reported: Vec<(Value, u64)> = if p.latest_only {
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
            .map(|(_, e)| (describe_event(e, labels.get(&e.subscription), now), changes[&e.subscription]))
            .collect()
    } else {
        events.iter().map(|e| (describe_event(e, labels.get(&e.subscription), now), 1)).collect()
    };
    let omitted = reported.len().saturating_sub(MAX_EVENTS);
    reported.drain(..omitted);

    let listed: Vec<Value> = reported
        .into_iter()
        .map(|(mut event, changes)| {
            if p.latest_only {
                event["changes"] = json!(changes);
            }
            event
        })
        .collect();
    let mut out = json!({"events": listed, "watching": watches.watching()});
    if omitted > 0 {
        out["omitted"] = json!(omitted);
        out["note"] = json!(format!(
            "only the latest {MAX_EVENTS} are listed; use latest_only to see one entry per subscription"
        ));
    }
    if dropped > 0 {
        out["dropped"] = json!(dropped);
    }
    if !lost.is_empty() {
        out["lost"] = lost_report(&lost);
    } else if labels.is_empty() {
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
    if event.own {
        out.insert("own".into(), json!(true));
    }
    Value::Object(out)
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
