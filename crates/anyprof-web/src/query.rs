//! JSON for the viewer. Numbers and flags come from the query string.
//! Profile text is encoded by `serde_json`, then the page assigns it with
//! `textContent`.

use anyprof_export::{Query, Trace, activity, aggregate};
use serde_json::{Value, json};

pub(crate) fn session_json(name: &str, trace: &Trace) -> Vec<u8> {
    let body = json!({
        "name": name,
        "format": trace.format,
        "weightKind": trace.weight_kind.as_str(),
        "unit": trace.unit,
        "timeline": trace.timeline.map(|(start, end)| json!({"start": start, "end": end})),
        "timelineDisabled": trace.timeline_disabled,
        "cpuNs": trace.cpu_ns,
        "wallNs": trace.wall_ns,
        "periodNs": trace.period_ns,
        "total": trace.total_weight(),
        "samples": trace.sample_records(),
        "hasSystemFrames": trace.has_system_frames(),
        "notes": trace.notes,
        "threads": trace.threads.iter().enumerate().map(|(id, thread)| json!({
            "id": id,
            "name": thread.name,
            "kind": thread.kind.as_str(),
        })).collect::<Vec<_>>(),
        "frames": trace.frames.iter().map(|frame| json!({
            "name": frame.name,
            "file": frame.file,
            "line": frame.line,
            "column": frame.column,
            "runtime": frame.runtime.as_str(),
            "module": frame.module,
            "system": frame.system,
        })).collect::<Vec<_>>(),
    });
    to_bytes(&body)
}

pub(crate) fn tree_json(trace: &Trace, query: &str) -> Result<Vec<u8>, String> {
    let query = parse_query(query)?;
    let tree = aggregate(trace, &query);
    let body = json!({
        "total": tree.total,
        "samples": tree.samples,
        "nodes": tree.nodes.iter().map(|node| json!({
            "id": node.id,
            "parent": node.parent,
            "frame": node.frame,
            "depth": node.depth,
            "weight": node.weight,
            "self": node.self_weight,
        })).collect::<Vec<_>>(),
        "functions": tree.functions.iter().map(|row| json!({
            "frame": row.frame,
            "weight": row.weight,
            "self": row.self_weight,
        })).collect::<Vec<_>>(),
        "heaviest": tree.heaviest,
    });
    Ok(to_bytes(&body))
}

pub(crate) fn timeline_json(trace: &Trace, query: &str) -> Result<Vec<u8>, String> {
    let (full_start, full_end) = trace.timeline.ok_or_else(|| {
        trace
            .timeline_disabled
            .clone()
            .unwrap_or_else(|| "this profile has no timeline".to_string())
    })?;
    let params = Params::parse(query)?;
    let start = params.u64("start")?.unwrap_or(full_start);
    let end = params.u64("end")?.unwrap_or(full_end);
    let buckets = params.usize("buckets")?.unwrap_or(200);
    let activity = activity(trace, start, end, buckets).map_err(|err| err.to_string())?;
    Ok(to_bytes(&json!({
        "start": activity.start,
        "end": activity.end,
        "rows": activity.rows,
    })))
}

fn to_bytes(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

fn parse_query(query: &str) -> Result<Query, String> {
    let params = Params::parse(query)?;
    let mut parsed = Query {
        start: params.u64("start")?,
        end: params.u64("end")?,
        threads: None,
        invert: params.flag("invert")?,
        hide_system: params.flag("hideSystem")?,
    };
    if let Some(value) = params.get("threads") {
        let mut ids = Vec::new();
        if !value.is_empty() {
            for piece in value.split(',') {
                let id = piece
                    .parse::<u32>()
                    .map_err(|_| format!("invalid thread id {piece}"))?;
                ids.push(id);
            }
        }
        parsed.threads = Some(ids);
    }
    Ok(parsed)
}

struct Params {
    pairs: Vec<(String, String)>,
}

impl Params {
    fn parse(query: &str) -> Result<Self, String> {
        let mut pairs = Vec::new();
        if query.is_empty() {
            return Ok(Self { pairs });
        }
        for piece in query.split('&') {
            if piece.is_empty() {
                continue;
            }
            let (key, value) = piece.split_once('=').unwrap_or((piece, ""));
            pairs.push((decode(key)?, decode(value)?));
        }
        Ok(Self { pairs })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn u64(&self, name: &str) -> Result<Option<u64>, String> {
        let Some(value) = self.get(name) else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }
        value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("invalid {name}"))
    }

    fn usize(&self, name: &str) -> Result<Option<usize>, String> {
        match self.u64(name)? {
            None => Ok(None),
            Some(value) => usize::try_from(value)
                .map(Some)
                .map_err(|_| format!("invalid {name}")),
        }
    }

    fn flag(&self, name: &str) -> Result<bool, String> {
        match self.get(name).unwrap_or("0") {
            "1" | "true" => Ok(true),
            "0" | "false" | "" => Ok(false),
            _ => Err(format!("invalid {name}")),
        }
    }
}

fn decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .map_err(|_| "invalid percent-encoding".to_string())?;
                let byte = u8::from_str_radix(hex, 16)
                    .map_err(|_| "invalid percent-encoding".to_string())?;
                out.push(byte);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "query is not utf-8".to_string())
}
