//! `device_parameters` and `set_parameter`.

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::live::{ItemRef, Live, TrackRef, call, field, field_bool, field_f64, field_str, items, set};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceParametersParams {
    pub track: TrackRef,
    /// Device index on the track (0-based, in chain order) or exact device name.
    pub device: ItemRef,
    /// Only parameters whose name contains this (case-insensitive).
    pub query: Option<String>,
}

const PARAM_FIELDS: &[&str] = &["name", "value", "min", "max", "is_quantized", "value_items"];

pub async fn device_parameters(live: &Live, params: DeviceParametersParams) -> Result<Value> {
    let track = params.track.resolve(live).await?;
    let (device_index, device_name) = params.device.resolve(live, &track.target.join("devices"), "device").await?;
    let list = track.target.join(&format!("devices {device_index} parameters"));
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
            if field_bool(p, "is_quantized") == Some(true) {
                entry.insert("options".into(), field(p, "value_items").cloned().unwrap_or_default());
            } else {
                entry.insert("min".into(), json!(field_f64(p, "min")));
                entry.insert("max".into(), json!(field_f64(p, "max")));
            }
            Value::Object(entry)
        })
        .collect();
    Ok(json!({"track": track.label(), "device": device_index, "device_name": device_name, "parameters": out}))
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
    /// Device index on the track or exact device name.
    pub device: ItemRef,
    /// Parameter index or exact parameter name (see `device_parameters`).
    pub parameter: ItemRef,
    /// A number within the parameter's min–max, or for a parameter with options, one of the options.
    pub value: ParamValue,
}

pub async fn set_parameter(live: &Live, params: SetParameterParams) -> Result<Value> {
    let track = params.track.resolve(live).await?;
    let (device_index, _) = params.device.resolve(live, &track.target.join("devices"), "device").await?;
    let list = track.target.join(&format!("devices {device_index} parameters"));
    let (param_index, param_name) = params.parameter.resolve(live, &list, "parameter").await?;
    let param = list.join(&param_index.to_string());
    let info = live.get_fields(&param, &["min", "max", "is_quantized", "value_items"]).await?;
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
        "device": device_index,
        "parameter": param_index,
        "name": param_name,
        "value": results[0]["value"],
        "display": results[1],
    }))
}
