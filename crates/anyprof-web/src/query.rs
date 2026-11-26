//! JSON for the viewer. Numbers and flags come from the query string.
//! Profile text is encoded by `serde_json`, then the page assigns it with
//! `textContent`.

use anyprof_export::{DiffQuery, DiffTrace, Query, Trace, activity, aggregate, diff_aggregate};
use serde_json::{Value, json};

pub(crate) fn session_json(name: &str, trace: &Trace) -> Vec<u8> {
    let body = json!({
        "mode": "single",
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
            "address": frame.address.map(|addr| format!("0x{addr:x}")),
            "system": frame.system,
        })).collect::<Vec<_>>(),
    });
    to_bytes(&body)
}

pub(crate) fn diff_session_json(
    name: &str,
    before_name: &str,
    after_name: &str,
    diff: &DiffTrace,
) -> Vec<u8> {
    let before_total = diff.before.total_weight().saturating_mul(diff.before_scale);
    let after_total = diff.after.total_weight().saturating_mul(diff.after_scale);
    let body = json!({
        "mode": "diff",
        "name": name,
        "format": format!("{} → {}", diff.before.format, diff.after.format),
        "before": {
            "name": before_name,
            "format": diff.before.format,
            "total": before_total,
            "samples": diff.before.sample_records(),
            "wallNs": diff.before.wall_ns,
            "cpuNs": diff.before.cpu_ns,
        },
        "after": {
            "name": after_name,
            "format": diff.after.format,
            "total": after_total,
            "samples": diff.after.sample_records(),
            "wallNs": diff.after.wall_ns,
            "cpuNs": diff.after.cpu_ns,
        },
        "weightKind": diff.weight_kind.as_str(),
        "unit": diff.unit,
        "timeline": Value::Null,
        "timelineDisabled": "Differential view compares aggregated weights between baseline and comparison profiles.",
        "total": after_total,
        "samples": diff.before.sample_records().saturating_add(diff.after.sample_records()),
        "hasSystemFrames": diff.has_system_frames(),
        "notes": diff.notes,
        "threads": [],
        "frames": diff.frames.iter().map(|frame| json!({
            "name": frame.name,
            "file": frame.file,
            "line": frame.line,
            "column": frame.column,
            "runtime": frame.runtime.as_str(),
            "module": frame.module,
            "address": frame.address.map(|addr| format!("0x{addr:x}")),
            "system": frame.system,
        })).collect::<Vec<_>>(),
    });
    to_bytes(&body)
}

pub(crate) fn diff_tree_json(diff: &DiffTrace, query: &str) -> Result<Vec<u8>, String> {
    let params = Params::parse(query)?;
    let filter = DiffQuery {
        invert: params.flag("invert")?,
        hide_system: params.flag("hideSystem")?,
    };
    let agg = diff_aggregate(diff, &filter);
    let clamp_delta = |d: i128| d.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    let body = json!({
        "mode": "diff",
        "total": agg.comparison_total,
        "baselineTotal": agg.baseline_total,
        "comparisonTotal": agg.comparison_total,
        "deltaTotal": clamp_delta(agg.delta_total()),
        "spanTotal": agg.span_total,
        "baselineSamples": agg.baseline_samples,
        "comparisonSamples": agg.comparison_samples,
        "samples": agg.baseline_samples.saturating_add(agg.comparison_samples),
        "nodes": agg.nodes.iter().map(|node| json!({
            "id": node.id,
            "parent": node.parent,
            "frame": node.frame,
            "depth": node.depth,
            "weight": node.comparison,
            "self": node.comparison_self,
            "baseline": node.baseline,
            "comparison": node.comparison,
            "baselineSelf": node.baseline_self,
            "comparisonSelf": node.comparison_self,
            "delta": clamp_delta(node.delta_total()),
            "deltaSelf": clamp_delta(node.delta_self()),
            "span": node.span,
        })).collect::<Vec<_>>(),
        "functions": agg.functions.iter().map(|row| json!({
            "frame": row.frame,
            "weight": row.comparison,
            "self": row.comparison_self,
            "baseline": row.baseline,
            "comparison": row.comparison,
            "baselineSelf": row.baseline_self,
            "comparisonSelf": row.comparison_self,
            "delta": clamp_delta(row.delta_total()),
            "deltaSelf": clamp_delta(row.delta_self()),
        })).collect::<Vec<_>>(),
        "heaviest": [],
    });
    Ok(to_bytes(&body))
}

pub(crate) fn diff_source_json(diff: &DiffTrace, query: &str) -> Result<Vec<u8>, String> {
    let params = Params::parse(query)?;
    let frame_idx = params
        .usize("frame")?
        .ok_or_else(|| "missing frame".to_string())?;
    let frame = diff
        .frames
        .get(frame_idx)
        .ok_or_else(|| format!("unknown frame {frame_idx}"))?;
    let hide_system = params.flag("hideSystem")?;

    let Some(raw_file) = frame.file.as_deref().filter(|path| !path.trim().is_empty()) else {
        return Ok(to_bytes(&json!({
            "available": false,
            "mode": "diff",
            "file": Value::Null,
            "line": frame.line,
            "column": frame.column,
            "reason": "This frame does not record a source file.",
            "hotLines": [],
            "lines": [],
        })));
    };

    let target_norm = normalize_source_key(raw_file);
    let before_agg = aggregate(
        &diff.before,
        &Query {
            hide_system,
            ..Query::default()
        },
    );
    let after_agg = aggregate(
        &diff.after,
        &Query {
            hide_system,
            ..Query::default()
        },
    );

    // `(baseline, comparison, baseline_self, comparison_self)` per 1-based line.
    let mut line_weights: std::collections::BTreeMap<u32, (u64, u64, u64, u64)> =
        std::collections::BTreeMap::new();
    for row in &before_agg.functions {
        let Some(cand) = diff.before.frames.get(row.frame as usize) else {
            continue;
        };
        if let (Some(cand_file), Some(line_no)) =
            (cand.file.as_deref(), cand.line.filter(|&l| l > 0))
            && normalize_source_key(cand_file) == target_norm
        {
            let e = line_weights.entry(line_no).or_insert((0, 0, 0, 0));
            e.0 =
                e.0.saturating_add(row.weight.saturating_mul(diff.before_scale));
            e.2 =
                e.2.saturating_add(row.self_weight.saturating_mul(diff.before_scale));
        }
    }
    for row in &after_agg.functions {
        let Some(cand) = diff.after.frames.get(row.frame as usize) else {
            continue;
        };
        if let (Some(cand_file), Some(line_no)) =
            (cand.file.as_deref(), cand.line.filter(|&l| l > 0))
            && normalize_source_key(cand_file) == target_norm
        {
            let e = line_weights.entry(line_no).or_insert((0, 0, 0, 0));
            e.1 =
                e.1.saturating_add(row.weight.saturating_mul(diff.after_scale));
            e.3 =
                e.3.saturating_add(row.self_weight.saturating_mul(diff.after_scale));
        }
    }

    let clamp_delta = |b: u64, a: u64| {
        (i128::from(a) - i128::from(b)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    };
    let hot_lines: Vec<Value> = line_weights
        .iter()
        .filter(|(_, (b, a, bs, as_))| *b > 0 || *a > 0 || *bs > 0 || *as_ > 0)
        .map(|(line, (b, a, bs, as_))| {
            json!({
                "line": line,
                "weight": a,
                "self": as_,
                "baseline": b,
                "comparison": a,
                "baselineSelf": bs,
                "comparisonSelf": as_,
                "delta": clamp_delta(*b, *a),
                "deltaSelf": clamp_delta(*bs, *as_),
            })
        })
        .collect();

    let path = match resolve_local_source_path(raw_file) {
        Ok(path) => path,
        Err(reason) => {
            return Ok(to_bytes(&json!({
                "available": false,
                "mode": "diff",
                "file": raw_file,
                "line": frame.line,
                "column": frame.column,
                "reason": reason,
                "hotLines": hot_lines,
                "lines": [],
            })));
        }
    };

    match read_local_source(&path) {
        Ok(text) => {
            let all_lines: Vec<&str> = text.lines().collect();
            let total_lines = all_lines.len();
            let (start_idx, end_idx) = if total_lines <= MAX_FULL_SOURCE_LINES {
                (0, total_lines)
            } else {
                let center = frame
                    .line
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(total_lines.saturating_sub(1) as u32)
                    as usize;
                let lo = center.saturating_sub(SOURCE_WINDOW_RADIUS);
                let hi = (center + SOURCE_WINDOW_RADIUS + 1).min(total_lines);
                (lo, hi)
            };
            let lines: Vec<Value> = all_lines[start_idx..end_idx]
                .iter()
                .enumerate()
                .map(|(offset, line_text)| {
                    let line_no = (start_idx + offset + 1) as u32;
                    let (b, a, bs, as_) =
                        line_weights.get(&line_no).copied().unwrap_or((0, 0, 0, 0));
                    json!({
                        "line": line_no,
                        "text": line_text,
                        "weight": a,
                        "self": as_,
                        "baseline": b,
                        "comparison": a,
                        "baselineSelf": bs,
                        "comparisonSelf": as_,
                        "delta": clamp_delta(b, a),
                        "deltaSelf": clamp_delta(bs, as_),
                    })
                })
                .collect();
            Ok(to_bytes(&json!({
                "available": true,
                "mode": "diff",
                "file": raw_file,
                "resolvedPath": path.display().to_string(),
                "line": frame.line,
                "column": frame.column,
                "totalLines": total_lines,
                "startLine": start_idx + 1,
                "hotLines": hot_lines,
                "lines": lines,
            })))
        }
        Err(reason) => Ok(to_bytes(&json!({
            "available": false,
            "mode": "diff",
            "file": raw_file,
            "line": frame.line,
            "column": frame.column,
            "reason": reason,
            "hotLines": hot_lines,
            "lines": [],
        }))),
    }
}

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FULL_SOURCE_LINES: usize = 1_500;
const SOURCE_WINDOW_RADIUS: usize = 600;

pub(crate) fn source_json(trace: &Trace, query: &str) -> Result<Vec<u8>, String> {
    let params = Params::parse(query)?;
    let frame_idx = params
        .usize("frame")?
        .ok_or_else(|| "missing frame".to_string())?;
    let frame = trace
        .frames
        .get(frame_idx)
        .ok_or_else(|| format!("unknown frame {frame_idx}"))?;

    let mut filter = parse_query(query)?;
    filter.invert = false;
    let agg = aggregate(trace, &filter);

    let Some(raw_file) = frame.file.as_deref().filter(|path| !path.trim().is_empty()) else {
        return Ok(to_bytes(&json!({
            "available": false,
            "file": Value::Null,
            "line": frame.line,
            "column": frame.column,
            "reason": "This frame does not record a source file.",
            "hotLines": [],
            "lines": [],
        })));
    };

    let target_norm = normalize_source_key(raw_file);
    let mut line_weights: std::collections::BTreeMap<u32, (u64, u64)> =
        std::collections::BTreeMap::new();
    for row in &agg.functions {
        let Some(candidate) = trace.frames.get(row.frame as usize) else {
            continue;
        };
        let Some(cand_file) = candidate.file.as_deref() else {
            continue;
        };
        let Some(line_no) = candidate.line.filter(|line| *line > 0) else {
            continue;
        };
        if normalize_source_key(cand_file) == target_norm {
            let entry = line_weights.entry(line_no).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(row.weight);
            entry.1 = entry.1.saturating_add(row.self_weight);
        }
    }

    let hot_lines: Vec<Value> = line_weights
        .iter()
        .filter(|(_, (weight, self_w))| *weight > 0 || *self_w > 0)
        .map(|(line, (weight, self_w))| {
            json!({
                "line": line,
                "weight": weight,
                "self": self_w,
            })
        })
        .collect();

    let path = match resolve_local_source_path(raw_file) {
        Ok(path) => path,
        Err(reason) => {
            return Ok(to_bytes(&json!({
                "available": false,
                "file": raw_file,
                "line": frame.line,
                "column": frame.column,
                "reason": reason,
                "hotLines": hot_lines,
                "lines": [],
            })));
        }
    };

    match read_local_source(&path) {
        Ok(text) => {
            let all_lines: Vec<&str> = text.lines().collect();
            let total_lines = all_lines.len();
            let (start_idx, end_idx) = if total_lines <= MAX_FULL_SOURCE_LINES {
                (0, total_lines)
            } else {
                let center = frame
                    .line
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(total_lines.saturating_sub(1) as u32)
                    as usize;
                let lo = center.saturating_sub(SOURCE_WINDOW_RADIUS);
                let hi = (center + SOURCE_WINDOW_RADIUS + 1).min(total_lines);
                (lo, hi)
            };
            let lines: Vec<Value> = all_lines[start_idx..end_idx]
                .iter()
                .enumerate()
                .map(|(offset, line_text)| {
                    let line_no = (start_idx + offset + 1) as u32;
                    let (weight, self_w) = line_weights.get(&line_no).copied().unwrap_or((0, 0));
                    json!({
                        "line": line_no,
                        "text": line_text,
                        "weight": weight,
                        "self": self_w,
                    })
                })
                .collect();
            Ok(to_bytes(&json!({
                "available": true,
                "file": raw_file,
                "resolvedPath": path.display().to_string(),
                "line": frame.line,
                "column": frame.column,
                "totalLines": total_lines,
                "startLine": start_idx + 1,
                "hotLines": hot_lines,
                "lines": lines,
            })))
        }
        Err(reason) => Ok(to_bytes(&json!({
            "available": false,
            "file": raw_file,
            "line": frame.line,
            "column": frame.column,
            "reason": reason,
            "hotLines": hot_lines,
            "lines": [],
        }))),
    }
}

fn normalize_source_key(raw: &str) -> String {
    match resolve_local_source_path(raw) {
        Ok(path) => path.to_string_lossy().replace('\\', "/"),
        Err(_) => raw.trim().replace('\\', "/"),
    }
}

fn resolve_local_source_path(raw: &str) -> Result<std::path::PathBuf, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err("Source file path is empty.");
    }
    if trimmed.starts_with('<')
        || trimmed.starts_with("node:")
        || trimmed.starts_with("wasm:")
        || trimmed.starts_with("internal/")
    {
        return Err("Runtime-internal or virtual module has no local source file on disk.");
    }
    let decoded = if let Some(rest) = trimmed.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost/").map_or(rest, |tail| tail);
        let unescaped = decode(rest).map_err(|_| "Invalid file:// URL encoding.")?;
        if cfg!(windows) {
            let stripped = unescaped.strip_prefix('/').unwrap_or(&unescaped);
            stripped.to_string()
        } else if unescaped.starts_with('/') {
            unescaped
        } else {
            format!("/{unescaped}")
        }
    } else if trimmed.contains("://") {
        return Err("Remote URL source is not stored on local disk.");
    } else {
        trimmed.to_string()
    };

    let path = std::path::PathBuf::from(&decoded);
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err("Source paths containing '..' are refused.");
        }
    }
    Ok(path)
}

fn read_local_source(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let metadata = std::fs::metadata(path).map_err(|_| {
        format!(
            "Source file is not present on this machine ({}).",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err("Source path is not a regular file.".to_string());
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err("Source file is larger than 2 MiB.".to_string());
    }
    let file = std::fs::File::open(path)
        .map_err(|err| format!("Could not read {}: {err}", path.display()))?;
    let mut buf = Vec::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| format!("Could not read {}: {err}", path.display()))?;
    if buf.len() as u64 > MAX_SOURCE_BYTES {
        return Err("Source file is larger than 2 MiB.".to_string());
    }
    String::from_utf8(buf).map_err(|_| "Source file is not UTF-8 text.".to_string())
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
