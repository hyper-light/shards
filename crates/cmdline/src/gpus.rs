//! `--gpus`, as docker/cli v29.8.1 reads one (opts/gpus.go, `GpuOpts.Set`): a CSV record
//! of fields, each a count alone (`all` or a number) or `KEY=VALUE` of `driver`, `count`,
//! `device`, `capabilities` and `options`, each key once; in its words where it refuses.

use std::collections::BTreeMap;

/// A GPU request (moby's `container.DeviceRequest`), as the CLI sends dockerd one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuRequest {
    pub driver: String,
    /// How many GPUs: -1 for all of them.
    pub count: i64,
    /// The GPUs asked for by ID, where `device=` names them.
    pub device_ids: Option<Vec<String>>,
    /// What they must do: `gpu`, after what `capabilities=` names.
    pub capabilities: Vec<Vec<String>>,
    pub options: BTreeMap<String, String>,
}

/// `parseCount`: `all` as -1, else `strconv.Atoi`'s number.
fn count(s: &str) -> Result<i64, String> {
    if s == "all" {
        return Ok(-1);
    }
    crate::network::atoi(s)
        .map_err(|e| format!("invalid count ({s}): value must be either \"all\" or an integer: {e}"))
}

/// One `--gpus` value, read as `GpuOpts.Set` reads it.
pub fn gpu_request(value: &str) -> Result<GpuRequest, String> {
    let fields = crate::network::csv_first_record(value)?;
    let mut req = GpuRequest::default();
    let mut seen: Vec<&str> = Vec::new();
    for field in &fields {
        let (key, val) = match field.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (field.as_str(), None),
        };
        if seen.contains(&key) {
            return Err(format!("gpu request key '{key}' can be specified only once"));
        }
        seen.push(key);
        let Some(val) = val else {
            seen.push("count");
            req.count = count(key)?;
            continue;
        };
        match key {
            "driver" => req.driver = val.to_string(),
            "count" => req.count = count(val)?,
            "device" => req.device_ids = Some(val.split(',').map(str::to_string).collect()),
            "capabilities" => {
                let mut caps: Vec<String> = val.split(',').map(str::to_string).collect();
                caps.push("gpu".into());
                req.capabilities = vec![caps];
            }
            "options" => {
                let options = crate::network::csv_first_record(val)
                    .map_err(|e| format!("failed to read gpu options: {e}"))?;
                // ConvertKVStringsToMap: each `K=V`, or `K` alone as empty.
                req.options = options
                    .iter()
                    .map(|kv| match kv.split_once('=') {
                        Some((k, v)) => (k.to_string(), v.to_string()),
                        None => (kv.clone(), String::new()),
                    })
                    .collect();
            }
            _ => return Err(format!("unexpected key '{key}' in '{field}'")),
        }
    }
    if !seen.contains(&"count") && req.device_ids.is_none() {
        req.count = 1;
    }
    if req.capabilities.is_empty() {
        req.capabilities = vec![vec!["gpu".into()]];
    }
    Ok(req)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Requests read as the CLI reads them: a count alone, `all`, keys, defaults (one GPU,
    /// `gpu` capability), and its refusals in its words.
    #[test]
    fn gpu_requests_are_read() {
        let all = gpu_request("all").unwrap();
        assert_eq!(
            (all.count, all.capabilities.clone()),
            (-1, vec![vec!["gpu".to_string()]])
        );
        assert_eq!(gpu_request("2").unwrap().count, 2);
        let one = gpu_request("driver=nvidia").unwrap();
        assert_eq!((one.driver.as_str(), one.count), ("nvidia", 1));
        let devices = gpu_request("\"device=0,1\",capabilities=compute,options=a=b").unwrap();
        assert_eq!(devices.device_ids, Some(vec!["0".into(), "1".into()]));
        assert_eq!(devices.count, 0);
        assert_eq!(
            devices.capabilities,
            vec![vec!["compute".to_string(), "gpu".into()]]
        );
        assert_eq!(devices.options.get("a").map(String::as_str), Some("b"));
        assert_eq!(
            gpu_request("foo").unwrap_err(),
            "invalid count (foo): value must be either \"all\" or an integer: invalid syntax"
        );
        assert_eq!(
            gpu_request("all,count=2").unwrap_err(),
            "gpu request key 'count' can be specified only once"
        );
        assert_eq!(gpu_request("x=1").unwrap_err(), "unexpected key 'x' in 'x=1'");
    }
}
