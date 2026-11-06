//! `device_parameters` and `set_parameter`.

use anyhow::{Result, bail};
use live_mcp_protocol::Target;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::live::{
    ItemRef, Live, Track, TrackRef, call, field, field_bool, field_f64, field_str, get_fields, items, set,
};

/// A device on a track, or inside a Rack on it.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DeviceRef {
    /// A device on the track: its 0-based index in chain order, or its exact name.
    One(ItemRef),
    /// A device inside a Rack: a path that alternates device, chain, device, e.g.
    /// `["Bass Rack", "Sub", "Operator"]` or `[0, 1, 0]`. Drum Rack pads are chains too.
    Path(Vec<ItemRef>),
}

/// A device found by a [`DeviceRef`].
struct Device {
    target: Target,
    /// How tools report the device back: its index, or the path of indexes for a nested device.
    label: Value,
    name: String,
}

impl DeviceRef {
    async fn resolve(&self, live: &Live, track: &Track) -> Result<Device> {
        let steps = match self {
            DeviceRef::One(step) => std::slice::from_ref(step),
            DeviceRef::Path(steps) => steps.as_slice(),
        };
        if steps.len() % 2 == 0 {
            bail!(
                "a device path alternates device, chain, device and ends at a device, like \
                 [\"Bass Rack\", \"Sub\", \"Operator\"]; this one has {} steps",
                steps.len()
            );
        }
        let mut target = track.target.clone();
        let mut indexes = Vec::with_capacity(steps.len());
        let mut name = String::new();
        for (i, step) in steps.iter().enumerate() {
            if i % 2 == 0 {
                let (index, device_name) = step.resolve(live, &target.join("devices"), "device").await?;
                target = target.join(&format!("devices {index}"));
                indexes.push(index);
                name = device_name;
            } else {
                if !can_have_chains(live, &target).await? {
                    bail!("{name} is not a Rack, so it has no chains");
                }
                let (index, _) = step.resolve(live, &target.join("chains"), &format!("chain in {name}")).await?;
                target = target.join(&format!("chains {index}"));
                indexes.push(index);
            }
        }
        let label = match self {
            DeviceRef::One(_) => json!(indexes[0]),
            DeviceRef::Path(_) => json!(indexes),
        };
        Ok(Device { target, label, name })
    }
}

async fn can_have_chains(live: &Live, device: &Target) -> Result<bool> {
    Ok(field_bool(&live.get_fields(device, &["can_have_chains"]).await?, "can_have_chains") == Some(true))
}

/// A Rack's chains with the names of their devices, so a caller can build a path into them.
async fn rack_chains(live: &Live, device: &Target) -> Result<Value> {
    let chains = live.get_fields(&device.join("chains"), &["name"]).await?;
    let ops = (0..items(&chains).len()).map(|i| get_fields(&device.join(&format!("chains {i} devices")), &["name"]));
    let devices = live.batch(ops.collect(), false).await?;
    let out: Vec<Value> = items(&chains)
        .iter()
        .zip(devices)
        .enumerate()
        .map(|(i, (chain, devices))| {
            let names: Vec<&str> = items(&devices).iter().map(|d| field_str(d, "name")).collect();
            json!({"index": i, "name": field_str(chain, "name"), "devices": names})
        })
        .collect();
    Ok(json!(out))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceParametersParams {
    pub track: TrackRef,
    /// Device index or exact name, or for a device inside a Rack a path that alternates device, chain,
    /// device, like ["Bass Rack", "Sub", "Operator"]. `device_parameters` on a Rack lists its chains.
    pub device: DeviceRef,
    /// Only parameters whose name contains this (case-insensitive).
    pub query: Option<String>,
}

const PARAM_FIELDS: &[&str] = &["name", "value", "min", "max", "is_quantized", "value_items", "is_enabled"];

pub async fn device_parameters(live: &Live, params: DeviceParametersParams) -> Result<Value> {
    let track = params.track.resolve(live).await?;
    let device = params.device.resolve(live, &track).await?;
    let list = device.target.join("parameters");
    let all = live.get_fields(&list, PARAM_FIELDS).await?;
    let wanted = params.query.as_deref().map(str::to_lowercase);
    let selected: Vec<(usize, &Value)> = items(&all)
        .iter()
        .enumerate()
        .filter(|(_, p)| wanted.as_ref().is_none_or(|w| field_str(p, "name").to_lowercase().contains(w.as_str())))
        .collect();
    // Display strings ("440 Hz", "-6.0 dB") in one batch.
    let ops = selected
        .iter()
        .map(|(i, p)| {
            call(&list.join(&i.to_string()), "str_for_value", vec![json!(field_f64(p, "value").unwrap_or(0.0))])
        })
        .collect();
    let displays = live.batch_lenient(ops).await?;
    let out: Vec<Value> = selected
        .iter()
        .zip(displays)
        .map(|((i, p), display)| {
            let mut entry = Map::new();
            entry.insert("index".into(), json!(i));
            entry.insert("name".into(), json!(field_str(p, "name")));
            entry.insert("value".into(), json!(field_f64(p, "value")));
            if let Ok(Value::String(text)) = display {
                entry.insert("display".into(), json!(text));
            }
            if field_bool(p, "is_enabled") == Some(false) {
                entry.insert("enabled".into(), json!(false));
            }
            if field_bool(p, "is_quantized") == Some(true) {
                entry.insert("options".into(), field(p, "value_items").cloned().unwrap_or_default());
            } else {
                entry.insert("min".into(), json!(field_f64(p, "min")));
                entry.insert("max".into(), json!(field_f64(p, "max")));
            }
            Value::Object(entry)
        })
        .collect();
    let mut result =
        json!({"track": track.label(), "device": device.label, "device_name": device.name, "parameters": out});
    if can_have_chains(live, &device.target).await? {
        result["chains"] = rack_chains(live, &device.target).await?;
    }
    if let Some(query) = params.query.as_deref()
        && selected.is_empty()
    {
        // Devices name parameters their own way (Drift's filter cutoff is "LP Freq"), so say what exists.
        let names: Vec<&str> = items(&all).iter().map(|p| field_str(p, "name")).collect();
        result["note"] =
            json!(format!("no parameter name contains {query:?}; the device's parameters are listed in all_names"));
        result["all_names"] = json!(names);
    }
    Ok(result)
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ParamValue {
    Number(f64),
    Text(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetParameterParams {
    pub track: TrackRef,
    /// Device index or exact name, or for a device inside a Rack a path that alternates device, chain,
    /// device, like ["Bass Rack", "Sub", "Operator"]. `device_parameters` on a Rack lists its chains.
    pub device: DeviceRef,
    /// Parameter index or exact parameter name (see `device_parameters`).
    pub parameter: ItemRef,
    /// A number within the parameter's min–max, or for a parameter with options, one of the options.
    pub value: ParamValue,
}

pub async fn set_parameter(live: &Live, params: SetParameterParams) -> Result<Value> {
    let track = params.track.resolve(live).await?;
    let device = params.device.resolve(live, &track).await?;
    let list = device.target.join("parameters");
    let (param_index, param_name) = params.parameter.resolve(live, &list, "parameter").await?;
    let param = list.join(&param_index.to_string());
    let info = live.get_fields(&param, &["min", "max", "is_quantized", "value_items", "is_enabled"]).await?;
    if field_bool(&info, "is_enabled") == Some(false) {
        bail!(
            "{param_name} on {} is disabled: a Rack macro or a Max for Live device controls it. \
             Set that control instead.",
            device.name
        );
    }
    let options: Vec<String> = items(field(&info, "value_items").unwrap_or(&Value::Null))
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let value = match &params.value {
        ParamValue::Number(n) => *n,
        ParamValue::Text(text) => match options.iter().position(|o| o.eq_ignore_ascii_case(text.trim())) {
            Some(i) => i as f64,
            None => match text.trim().parse::<f64>() {
                Ok(n) => n,
                Err(_) if options.is_empty() => bail!("{param_name} takes a number, not {text:?}"),
                Err(_) => bail!("{param_name} has no option {text:?}; options are {options:?}"),
            },
        },
    };
    let (min, max) = (field_f64(&info, "min").unwrap_or(f64::MIN), field_f64(&info, "max").unwrap_or(f64::MAX));
    if !(min..=max).contains(&value) {
        bail!("{param_name} must be between {min} and {max}, got {value}");
    }
    let results = live
        .batch(vec![set(&param, "value", json!(value)), call(&param, "str_for_value", vec![json!(value)])], true)
        .await?;
    Ok(json!({
        "track": track.label(),
        "device": device.label,
        "parameter": param_index,
        "name": param_name,
        "value": results[0]["value"],
        "display": results[1],
    }))
}
