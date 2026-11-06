//! `browser` (list or search Live's browser) and `load_device`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use live_mcp_protocol::Target;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::live::{Live, TrackRef, call, field_bool, field_str, get_fields, items, obj_arg, obj_id, set};

/// Browser roots, by their Live API attribute names.
pub const ROOTS: &[&str] = &[
    "instruments",
    "audio_effects",
    "midi_effects",
    "drums",
    "sounds",
    "plugins",
    "max_for_live",
    "samples",
    "clips",
    "packs",
    "user_library",
    "current_project",
];

/// Searched when the caller names no roots. Samples, clips and packs are large and rarely what is meant.
const DEFAULT_SEARCH_ROOTS: &[&str] =
    &["instruments", "audio_effects", "midi_effects", "drums", "sounds", "plugins", "max_for_live", "user_library"];

const ITEM_FIELDS: &[&str] = &["name", "is_folder", "is_loadable", "is_device", "uri"];
const MAX_DEPTH: usize = 8;
const MAX_ITEMS_PER_ROOT: usize = 30_000;
const FOLDERS_PER_BATCH: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Root first, then folder names, then the item's own name.
    pub path: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub folder: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub loadable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub device: bool,
}

/// Search indexes per root, built on first use. The browser rarely changes during a session.
#[derive(Default)]
pub struct BrowserCache {
    roots: Mutex<HashMap<String, Arc<RootIndex>>>,
}

struct RootIndex {
    entries: Vec<Entry>,
    truncated: bool,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BrowserParams {
    /// Folder to list, as names from a root, e.g. ["instruments", "Operator"]. Empty: list the roots.
    #[serde(default)]
    pub path: Vec<String>,
    /// Instead of listing: find items whose name contains all of these words (case-insensitive).
    pub query: Option<String>,
    /// Roots to search. Default: instruments, audio_effects, midi_effects, drums, sounds, plugins,
    /// max_for_live, user_library.
    pub roots: Option<Vec<String>>,
    /// Maximum results (default 50).
    pub limit: Option<usize>,
    /// Rebuild the search index (it is built once per session, per root).
    #[serde(default)]
    pub refresh: bool,
}

pub async fn browser(live: &Live, cache: &BrowserCache, params: BrowserParams) -> Result<Value> {
    let limit = params.limit.unwrap_or(50).max(1);
    if let Some(query) = params.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        return search(live, cache, query, params.roots.as_deref(), limit, params.refresh).await;
    }
    if params.path.is_empty() {
        let ops = ROOTS.iter().map(|r| get_fields(&root_target(r), &["name"])).collect();
        let available: Vec<&str> = ROOTS
            .iter()
            .zip(live.batch_lenient(ops).await?)
            .filter(|(_, r)| r.as_ref().is_ok_and(|v| !v.is_null()))
            .map(|(name, _)| *name)
            .collect();
        return Ok(json!({"roots": available}));
    }
    let folder = walk(live, &params.path).await?;
    let children = live.get_fields(&folder.join("children"), ITEM_FIELDS).await?;
    let all = items(&children);
    let listed: Vec<Value> = all
        .iter()
        .take(limit)
        .map(|c| {
            let mut path = params.path.clone();
            path.push(field_str(c, "name").to_owned());
            json!(entry_from(c, path))
        })
        .collect();
    Ok(json!({"path": params.path, "total": all.len(), "items": listed}))
}

fn root_target(root: &str) -> Target {
    Target::path(format!("live_app browser {root}"))
}

/// Normalises "Audio Effects" / "audio effects" / "audio_effects" to the attribute name.
fn root_name(name: &str) -> Result<&'static str> {
    let wanted = name.trim().to_lowercase().replace([' ', '-'], "_");
    let alias = match wanted.as_str() {
        "plug_ins" => "plugins",
        "max_for_live_devices" | "m4l" => "max_for_live",
        other => other,
    };
    ROOTS
        .iter()
        .copied()
        .find(|r| *r == alias)
        .ok_or_else(|| anyhow::anyhow!("unknown browser root {name:?}; roots are {ROOTS:?}"))
}

fn entry_from(item: &Value, path: Vec<String>) -> Entry {
    Entry {
        path,
        folder: field_bool(item, "is_folder") == Some(true),
        loadable: field_bool(item, "is_loadable") == Some(true),
        device: field_bool(item, "is_device") == Some(true),
    }
}

/// Resolves a browser path (root, then item names) to the item's target.
async fn walk(live: &Live, path: &[String]) -> Result<Target> {
    let Some((root, rest)) = path.split_first() else { bail!("empty browser path") };
    let mut current = root_target(root_name(root)?);
    for (depth, name) in rest.iter().enumerate() {
        let children = live.get_fields(&current.join("children"), &["name"]).await?;
        let wanted = name.to_lowercase();
        let found = items(&children)
            .iter()
            .find(|c| field_str(c, "name") == name)
            .or_else(|| items(&children).iter().find(|c| field_str(c, "name").to_lowercase() == wanted));
        let Some(id) = found.and_then(obj_id) else {
            let names: Vec<&str> = items(&children).iter().map(|c| field_str(c, "name")).take(50).collect();
            bail!("{name:?} not found in {:?}; it contains {names:?}", &path[..=depth]);
        };
        current = Target::id(id);
    }
    Ok(current)
}

async fn search(
    live: &Live,
    cache: &BrowserCache,
    query: &str,
    roots: Option<&[String]>,
    limit: usize,
    refresh: bool,
) -> Result<Value> {
    let roots: Vec<&'static str> = match roots {
        Some(names) if !names.is_empty() => names.iter().map(|n| root_name(n)).collect::<Result<_>>()?,
        _ => DEFAULT_SEARCH_ROOTS.to_vec(),
    };
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
    let mut hits: Vec<(u8, usize, Entry)> = Vec::new();
    let mut truncated = Vec::new();
    for root in roots {
        let index = root_index(live, cache, root, refresh).await?;
        if index.truncated {
            truncated.push(root);
        }
        for entry in &index.entries {
            let name = entry.path.last().map(|n| n.to_lowercase()).unwrap_or_default();
            if words.iter().all(|w| name.contains(w.as_str())) {
                // Rank: exact name, then loadable items, then shallower paths.
                let rank = if name == words.join(" ") {
                    0
                } else if entry.loadable {
                    1
                } else {
                    2
                };
                hits.push((rank, entry.path.len(), entry.clone()));
            }
        }
    }
    hits.sort_by_key(|h| (h.0, h.1));
    let total = hits.len();
    let results: Vec<Entry> = hits.into_iter().take(limit).map(|(_, _, e)| e).collect();
    let mut out = json!({"query": query, "total": total, "items": results});
    if !truncated.is_empty() {
        out["truncated_roots"] = json!(truncated);
    }
    Ok(out)
}

async fn root_index(live: &Live, cache: &BrowserCache, root: &'static str, refresh: bool) -> Result<Arc<RootIndex>> {
    let mut roots = cache.roots.lock().await;
    if !refresh && let Some(index) = roots.get(root) {
        return Ok(index.clone());
    }
    let index = Arc::new(build_index(live, root).await?);
    roots.insert(root.to_owned(), index.clone());
    Ok(index)
}

/// Breadth-first walk of one root, a batch of folders at a time.
async fn build_index(live: &Live, root: &'static str) -> Result<RootIndex> {
    let mut entries = Vec::new();
    let mut frontier: Vec<(Target, Vec<String>)> = vec![(root_target(root), vec![root.to_owned()])];
    let mut truncated = false;
    for _depth in 0..MAX_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for chunk in frontier.chunks(FOLDERS_PER_BATCH) {
            let ops = chunk.iter().map(|(t, _)| get_fields(&t.join("children"), ITEM_FIELDS)).collect();
            for ((_, path), children) in chunk.iter().zip(live.batch_lenient(ops).await?) {
                let Ok(children) = children else { continue };
                for child in items(&children) {
                    let mut child_path = path.clone();
                    child_path.push(field_str(child, "name").to_owned());
                    let entry = entry_from(child, child_path.clone());
                    // Devices are not folders but hold their presets as children
                    // (instruments → Analog → Bass → ...).
                    if (entry.folder || entry.device)
                        && let Some(id) = obj_id(child)
                    {
                        next.push((Target::id(id), child_path));
                    }
                    entries.push(entry);
                }
            }
            if entries.len() >= MAX_ITEMS_PER_ROOT {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
        frontier = next;
    }
    truncated |= !frontier.is_empty() && entries.len() >= MAX_ITEMS_PER_ROOT;
    Ok(RootIndex { entries, truncated })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadDeviceParams {
    pub track: TrackRef,
    /// The item's browser path, as `browser` reports it, e.g. ["audio_effects", "Reverb"].
    pub path: Vec<String>,
}

pub async fn load_device(live: &Live, params: LoadDeviceParams) -> Result<Value> {
    let track = params.track.resolve(live).await?;
    let item = walk(live, &params.path).await?;
    let loadable = live.get(&item.join("is_loadable")).await?;
    if loadable != true {
        bail!("{:?} is not loadable (it may be a folder; list it with `browser`)", params.path);
    }
    // Live loads browser items onto the selected track.
    live.batch(
        vec![
            set(&Target::path("live_set view"), "selected_track", obj_arg(&track.target)),
            call(&Target::path("live_app browser"), "load_item", vec![obj_arg(&item)]),
        ],
        true,
    )
    .await?;
    let devices = live.get_fields(&track.target.join("devices"), &["name"]).await?;
    let names: Vec<&str> = items(&devices).iter().map(|d| field_str(d, "name")).collect();
    Ok(json!({"track": track.label(), "loaded": params.path.last(), "devices": names}))
}
