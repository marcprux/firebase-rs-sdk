use serde_json::{Map, Number, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{internal_error, invalid_argument, DatabaseResult};

/// Port of `serverTimestamp()` from
/// `packages/database/src/api/ServerValue.ts`.
pub fn server_timestamp() -> Value {
    serde_json::json!({ ".sv": "timestamp" })
}

/// Port of `increment()` from `packages/database/src/api/ServerValue.ts`.
///
/// # Arguments
/// * `delta` - Amount to atomically add to the current value.
pub fn increment(delta: f64) -> Value {
    serde_json::json!({
        ".sv": {
            "increment": delta,
        }
    })
}

pub(crate) fn contains_server_value(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.contains_key(".sv") {
                return true;
            }
            map.values().any(contains_server_value)
        }
        Value::Array(items) => items.iter().any(contains_server_value),
        _ => false,
    }
}

pub(crate) fn resolve_server_values(value: Value, current: Option<&Value>) -> DatabaseResult<Value> {
    match value {
        Value::Object(mut map) => {
            if let Some(spec) = map.remove(".sv") {
                return resolve_server_placeholder(spec, current.map(extract_data_ref));
            }
            let mut resolved = Map::with_capacity(map.len());
            for (key, child) in map.into_iter() {
                let child_current = current
                    .and_then(|curr| match curr {
                        Value::Object(obj) => obj.get(&key),
                        Value::Array(arr) => key.parse::<usize>().ok().and_then(|idx| arr.get(idx)),
                        _ => None,
                    })
                    .map(extract_data_ref);
                let child_resolved = resolve_server_values(child, child_current)?;
                resolved.insert(key, child_resolved);
            }
            Ok(Value::Object(resolved))
        }
        Value::Array(items) => {
            let mut resolved = Vec::with_capacity(items.len());
            for (index, child) in items.into_iter().enumerate() {
                let child_current = current
                    .and_then(|curr| match curr {
                        Value::Array(arr) => arr.get(index),
                        _ => None,
                    })
                    .map(extract_data_ref);
                resolved.push(resolve_server_values(child, child_current)?);
            }
            Ok(Value::Array(resolved))
        }
        other => Ok(other),
    }
}

pub(crate) fn resolve_server_placeholder(spec: Value, current: Option<&Value>) -> DatabaseResult<Value> {
    match spec {
        Value::String(token) if token == "timestamp" => {
            let millis = current_time_millis()?;
            Ok(Value::Number(Number::from(millis)))
        }
        Value::Object(mut map) => {
            if let Some(delta) = map.remove("increment") {
                let delta = delta
                    .as_f64()
                    .ok_or_else(|| invalid_argument("ServerValue.increment delta must be numeric"))?;
                let base = current
                    .and_then(|value| match value {
                        Value::Number(number) => number.as_f64(),
                        _ => None,
                    })
                    .unwrap_or(0.0);
                let total = base + delta;
                let number = Number::from_f64(total)
                    .ok_or_else(|| invalid_argument("ServerValue.increment produced an invalid number"))?;
                Ok(Value::Number(number))
            } else {
                Err(invalid_argument("Unsupported server value placeholder"))
            }
        }
        _ => Err(invalid_argument("Unsupported server value placeholder")),
    }
}

pub(crate) fn extract_data_ref<'a>(value: &'a Value) -> &'a Value {
    value.as_object().and_then(|obj| obj.get(".value")).unwrap_or(value)
}

pub(crate) fn current_time_millis() -> DatabaseResult<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| internal_error("System time is before the Unix epoch"))?;
    let millis = duration.as_millis();
    millis
        .try_into()
        .map_err(|_| internal_error("Timestamp exceeds 64-bit range"))
}
