//! V8 `.cpuprofile` JSON, the document `Profiler.stop` returns from Node and Chrome.
//!
//! Node ids form a tree. `samples` names the leaf of each sample and `timeDeltas`
//! is that sample's weight in microseconds. Call-frame line and column numbers
//! are 0-based; the unified profile uses 1-based lines. `(root)` frames are
//! dropped. A `[runtime] name` prefix on a function is honored; otherwise every
//! frame is stamped with the runtime passed to [`import_cpuprofile`].

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyprof_core::{Frame, Profile, Runtime, Stack};
use serde_json::{Value, json};

use crate::SampleCounts;
use crate::error::Error;
use crate::symbol::split_runtime_prefix;

/// Decode a `.cpuprofile` document, or a CDP `Profiler.stop` result that wraps one.
pub fn import_cpuprofile(bytes: &[u8], default_runtime: Runtime) -> Result<Profile, Error> {
    let value = parse_json(bytes)?;
    let root = document_root(&value)?;
    let parsed = parse_nodes(node_array(root)?)?;
    let samples = number_array(root.get("samples"));
    let deltas = time_deltas(root.get("timeDeltas"));
    let stacks = if samples.is_empty() {
        stacks_from_hit_counts(&parsed, default_runtime)?
    } else {
        stacks_from_samples(&parsed, &samples, &deltas, default_runtime)?
    };

    let start = root.get("startTime").and_then(Value::as_u64).unwrap_or(0);
    let end = root.get("endTime").and_then(Value::as_u64).unwrap_or(0);
    let duration = end
        .checked_sub(start)
        .map(Duration::from_micros)
        .unwrap_or_default();
    Ok(Profile {
        duration,
        stacks,
        ..Profile::default()
    })
}

/// Count V8 samples, and those whose leaf is `(idle)` or `(program)`.
///
/// Falls back to `hitCount` when the document has no `samples` array.
pub(crate) fn sample_counts(bytes: &[u8]) -> Result<SampleCounts, Error> {
    let value = parse_json(bytes)?;
    let root = document_root(&value)?;
    let mut idle_nodes = HashSet::new();
    let mut hits = SampleCounts::default();
    for node in node_array(root)? {
        let name = node
            .pointer("/callFrame/functionName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let idle = is_idle_name(name);
        if idle && let Some(id) = node.get("id").and_then(Value::as_u64) {
            idle_nodes.insert(id);
        }
        let hit = node.get("hitCount").and_then(Value::as_u64).unwrap_or(0);
        hits.total = hits.total.saturating_add(hit);
        if idle {
            hits.idle = hits.idle.saturating_add(hit);
        }
    }
    let samples = number_array(root.get("samples"));
    if samples.is_empty() {
        return Ok(hits);
    }
    Ok(SampleCounts {
        total: samples.len() as u64,
        idle: samples.iter().filter(|id| idle_nodes.contains(id)).count() as u64,
    })
}

/// V8's pseudo-frames for a thread with no JavaScript or native work on the stack.
pub(crate) fn is_idle_name(name: &str) -> bool {
    matches!(name, "(idle)" | "(program)")
}

pub(crate) fn export(profile: &Profile) -> Result<Vec<u8>, Error> {
    // Node and browser capture keep Profiler.stop's JSON. Re-encoding it here
    // would collapse samples and drop scriptId, which Chrome DevTools then
    // shows as an empty profile. `convert` has no raw document and rebuilds one.
    if let Some(raw) = profile
        .raw_cpuprofile
        .as_ref()
        .filter(|raw| !raw.is_empty())
    {
        return Ok(raw.clone());
    }
    let mut builder = Trie::with_root();
    let mut samples = Vec::new();
    let mut deltas = Vec::new();
    for stack in &profile.stacks {
        if stack.weight == 0 {
            continue;
        }
        let mut parent = 1_u64;
        let mut leaf = 1_u64;
        let mut saw_frame = false;
        for frame in stack.frames.iter().rev() {
            if frame.name == "(root)" {
                continue;
            }
            parent = builder.intern(parent, frame);
            leaf = parent;
            saw_frame = true;
        }
        if !saw_frame {
            continue;
        }
        samples.push(leaf);
        deltas.push(stack.weight);
    }
    let end = deltas
        .iter()
        .fold(0_u64, |total, weight| total.saturating_add(*weight));
    let value = json!({
        "nodes": builder.nodes(),
        "startTime": 0,
        "endTime": end,
        "samples": samples,
        "timeDeltas": deltas,
    });
    let mut bytes = serde_json::to_vec_pretty(&value)
        .map_err(|err| Error::invalid("cpuprofile", format!("failed to encode JSON: {err}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

struct ParsedNode {
    frame: Frame,
    /// `functionName` had no `[runtime] ` prefix, so the caller stamps a runtime.
    unprefixed: bool,
    parent: Option<u64>,
    hit_count: u64,
}

struct Parsed {
    by_id: HashMap<u64, ParsedNode>,
}

fn parse_nodes(nodes: &[Value]) -> Result<Parsed, Error> {
    let mut by_id = HashMap::new();
    let mut children: Vec<(u64, Vec<u64>)> = Vec::new();
    for node in nodes {
        let id = node
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::invalid("cpuprofile", "node is missing id"))?;
        let call = node.get("callFrame").ok_or_else(|| {
            Error::invalid("cpuprofile", format!("node {id} is missing callFrame"))
        })?;
        let (frame, unprefixed) = frame_from_call(call);
        let hit_count = node.get("hitCount").and_then(Value::as_u64).unwrap_or(0);
        let child_ids = node
            .get("children")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        children.push((id, child_ids));
        by_id.insert(
            id,
            ParsedNode {
                frame,
                unprefixed,
                parent: None,
                hit_count,
            },
        );
    }
    for (parent, child_ids) in children {
        for child in child_ids {
            if let Some(node) = by_id.get_mut(&child) {
                node.parent = Some(parent);
            }
        }
    }
    Ok(Parsed { by_id })
}

fn frame_from_call(call: &Value) -> (Frame, bool) {
    let raw_name = call
        .get("functionName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (runtime, name, unprefixed) = match split_runtime_prefix(raw_name) {
        Some((runtime, name)) => (runtime, name, false),
        None => (Runtime::Native, raw_name, true),
    };
    let name = if name.is_empty() {
        "(anonymous)".to_string()
    } else {
        name.to_string()
    };
    let url = call.get("url").and_then(Value::as_str).unwrap_or("");
    let frame = Frame {
        runtime,
        name,
        file: if url.is_empty() {
            None
        } else {
            Some(url.to_string())
        },
        line: source_position(call.get("lineNumber")),
        column: source_position(call.get("columnNumber")),
        module: None,
        address: None,
    };
    (frame, unprefixed)
}

fn source_position(value: Option<&Value>) -> Option<u32> {
    let number = value?.as_i64()?;
    if number < 0 {
        None
    } else {
        u32::try_from(number)
            .ok()
            .map(|number| number.saturating_add(1))
    }
}

fn stacks_from_samples(
    parsed: &Parsed,
    samples: &[u64],
    deltas: &[u64],
    default_runtime: Runtime,
) -> Result<Vec<Stack>, Error> {
    let mut order = Vec::new();
    let mut totals: HashMap<Vec<Frame>, u64> = HashMap::new();
    for (index, sample) in samples.iter().enumerate() {
        let weight = deltas
            .get(index)
            .copied()
            .filter(|weight| *weight > 0)
            .unwrap_or(1);
        let frames = walk_stack(parsed, *sample, default_runtime)?;
        if frames.is_empty() {
            continue;
        }
        add_stack(&mut order, &mut totals, frames, weight);
    }
    Ok(finish(order, totals))
}

fn stacks_from_hit_counts(parsed: &Parsed, default_runtime: Runtime) -> Result<Vec<Stack>, Error> {
    let mut order = Vec::new();
    let mut totals: HashMap<Vec<Frame>, u64> = HashMap::new();
    for id in parsed.by_id.keys() {
        let hit = parsed.by_id[id].hit_count;
        if hit == 0 {
            continue;
        }
        let frames = walk_stack(parsed, *id, default_runtime)?;
        if frames.is_empty() {
            continue;
        }
        add_stack(&mut order, &mut totals, frames, hit);
    }
    Ok(finish(order, totals))
}

fn walk_stack(parsed: &Parsed, mut id: u64, default_runtime: Runtime) -> Result<Vec<Frame>, Error> {
    let mut frames = Vec::new();
    for _ in 0..2048 {
        let Some(node) = parsed.by_id.get(&id) else {
            return Err(Error::invalid(
                "cpuprofile",
                format!("sample references unknown node {id}"),
            ));
        };
        let mut frame = node.frame.clone();
        if node.unprefixed {
            frame.runtime = default_runtime;
        }
        if frame.name != "(root)" {
            frames.push(frame);
        }
        match node.parent {
            Some(parent) => id = parent,
            None => return Ok(frames),
        }
    }
    Err(Error::invalid(
        "cpuprofile",
        "node parent chain is deeper than 2048 frames",
    ))
}

fn add_stack(
    order: &mut Vec<Vec<Frame>>,
    totals: &mut HashMap<Vec<Frame>, u64>,
    frames: Vec<Frame>,
    weight: u64,
) {
    if let Some(total) = totals.get_mut(&frames) {
        *total = total.saturating_add(weight);
    } else {
        totals.insert(frames.clone(), weight);
        order.push(frames);
    }
}

fn finish(order: Vec<Vec<Frame>>, totals: HashMap<Vec<Frame>, u64>) -> Vec<Stack> {
    order
        .into_iter()
        .map(|frames| Stack {
            weight: totals[&frames],
            frames,
        })
        .collect()
}

fn parse_json(bytes: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice(bytes)
        .map_err(|err| Error::invalid("cpuprofile", format!("not JSON: {err}")))
}

fn document_root(value: &Value) -> Result<&Value, Error> {
    if value.get("nodes").is_some() {
        Ok(value)
    } else if value
        .get("profile")
        .and_then(|profile| profile.get("nodes"))
        .is_some()
    {
        Ok(&value["profile"])
    } else {
        Err(Error::invalid(
            "cpuprofile",
            "missing nodes array (expected a V8 CPU profile)",
        ))
    }
}

fn node_array(root: &Value) -> Result<&[Value], Error> {
    root.get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| Error::invalid("cpuprofile", "nodes is not an array"))
}

/// V8 occasionally records a negative delta when samples arrive out of order.
/// Keep one entry per sample so later deltas stay aligned with `samples`.
fn time_deltas(value: Option<&Value>) -> Vec<u64> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|delta| {
                    delta
                        .as_i64()
                        .map_or(0, |delta| delta.max(0).unsigned_abs())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn number_array(value: Option<&Value>) -> Vec<u64> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

struct Trie {
    nodes: Vec<TrieNode>,
    index: HashMap<(u64, FrameKey), u64>,
}

struct TrieNode {
    frame: Frame,
    children: Vec<u64>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FrameKey {
    runtime: Runtime,
    name: String,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

impl Trie {
    fn with_root() -> Self {
        Self {
            nodes: vec![TrieNode {
                frame: Frame::new(Runtime::Native, "(root)"),
                children: Vec::new(),
            }],
            index: HashMap::new(),
        }
    }

    fn intern(&mut self, parent: u64, frame: &Frame) -> u64 {
        let name = if frame.name.is_empty() {
            "(anonymous)".to_string()
        } else {
            frame.name.clone()
        };
        let key = (
            parent,
            FrameKey {
                runtime: frame.runtime,
                name: name.clone(),
                file: frame.file.clone(),
                line: frame.line,
                column: frame.column,
            },
        );
        if let Some(id) = self.index.get(&key) {
            return *id;
        }
        let id = self.nodes.len() as u64 + 1;
        let mut stored = frame.clone();
        stored.name = name;
        self.nodes.push(TrieNode {
            frame: stored,
            children: Vec::new(),
        });
        self.nodes[parent as usize - 1].children.push(id);
        self.index.insert(key, id);
        id
    }

    fn nodes(&self) -> Vec<Value> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let (line, column, url) = if index == 0 {
                    (-1, -1, String::new())
                } else {
                    let line = node
                        .frame
                        .line
                        .map(|line| i64::from(line).saturating_sub(1))
                        .unwrap_or(-1);
                    let column = node
                        .frame
                        .column
                        .map(|column| i64::from(column).saturating_sub(1))
                        .unwrap_or(-1);
                    (line, column, node.frame.file.clone().unwrap_or_default())
                };
                // hitCount is omitted on purpose. Chrome treats a present hitCount
                // of 0 as authoritative and then divides by a total of zero, so the
                // chart is empty even when `samples` is populated.
                json!({
                    "id": index as u64 + 1,
                    "callFrame": {
                        "functionName": node.frame.name,
                        "scriptId": "0",
                        "url": url,
                        "lineNumber": line,
                        "columnNumber": column,
                    },
                    "children": node.children,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::import_cpuprofile;
    use crate::{Format, export, import};
    use anyprof_core::Runtime;
    use std::time::Duration;

    fn sample() -> &'static str {
        r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 0, "children": [2]},
            {"id": 2, "callFrame": {"functionName": "main", "url": "file:///app.js", "lineNumber": 0, "columnNumber": 0}, "hitCount": 0, "children": [3]},
            {"id": 3, "callFrame": {"functionName": "work", "url": "file:///app.js", "lineNumber": 9, "columnNumber": 4}, "hitCount": 0, "children": []}
          ],
          "startTime": 1000,
          "endTime": 4000,
          "samples": [3, 3, 2],
          "timeDeltas": [1000, 1000, 1000]
        }"#
    }

    #[test]
    fn converts_samples_into_leaf_first_stacks() {
        let profile = import_cpuprofile(sample().as_bytes(), Runtime::Node).unwrap();
        assert_eq!(profile.duration, Duration::from_micros(3000));
        assert_eq!(profile.stacks.len(), 2);
        assert_eq!(profile.total_weight(), 3000);

        let hot = &profile.stacks[0];
        assert_eq!(hot.weight, 2000);
        assert_eq!(hot.frames[0].name, "work");
        assert_eq!(hot.frames[0].runtime, Runtime::Node);
        assert_eq!(hot.frames[0].file.as_deref(), Some("file:///app.js"));
        assert_eq!(hot.frames[0].line, Some(10));
        assert_eq!(hot.frames[0].column, Some(5));
        assert_eq!(hot.frames[1].name, "main");
        assert_eq!(hot.frames[1].line, Some(1));
        assert!(hot.frames.iter().all(|frame| frame.name != "(root)"));

        assert_eq!(profile.stacks[1].weight, 1000);
        assert_eq!(profile.stacks[1].frames[0].name, "main");
    }

    #[test]
    fn collapsed_export_uses_the_unified_model() {
        let profile = import(sample().as_bytes(), Format::Cpuprofile).unwrap();
        let text = String::from_utf8(export(&profile, Format::Collapsed).unwrap()).unwrap();
        assert_eq!(
            text,
            "\
[node] main;[node] work 2000
[node] main 1000
"
        );
    }

    #[test]
    fn rebuilt_cpuprofile_is_a_devtools_tree() {
        let original = import_cpuprofile(sample().as_bytes(), Runtime::Browser).unwrap();
        assert_eq!(original.stacks[0].frames[0].runtime, Runtime::Browser);
        let bytes = export(&original, Format::Cpuprofile).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let nodes = value["nodes"].as_array().unwrap();
        let samples = value["samples"].as_array().unwrap();
        let deltas = value["timeDeltas"].as_array().unwrap();
        assert_eq!(samples.len(), deltas.len());
        assert!(value["startTime"].as_u64().is_some());
        assert!(value["endTime"].as_u64().unwrap() >= value["startTime"].as_u64().unwrap());
        assert_eq!(nodes[0]["callFrame"]["functionName"], "(root)");
        assert_eq!(nodes[0]["callFrame"]["scriptId"], "0");
        assert!(nodes[0].get("hitCount").is_none());

        let mut ids = std::collections::HashSet::new();
        let mut children = std::collections::HashSet::new();
        for node in nodes {
            let id = node["id"].as_u64().unwrap();
            assert!(ids.insert(id), "duplicate id {id}");
            for field in [
                "functionName",
                "scriptId",
                "url",
                "lineNumber",
                "columnNumber",
            ] {
                assert!(node["callFrame"].get(field).is_some(), "missing {field}");
            }
            for child in node["children"].as_array().unwrap() {
                children.insert(child.as_u64().unwrap());
            }
        }
        for sample in samples {
            let id = sample.as_u64().unwrap();
            assert!(ids.contains(&id), "sample {id} is not a node");
        }
        for node in nodes.iter().skip(1) {
            let id = node["id"].as_u64().unwrap();
            assert!(children.contains(&id), "node {id} is not a child of anyone");
        }
        let names: Vec<_> = nodes
            .iter()
            .map(|node| node["callFrame"]["functionName"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"work"));
        assert!(names.contains(&"main"));
        assert!(names.iter().all(|name| !name.starts_with('[')));

        let again = import_cpuprofile(&bytes, Runtime::Browser).unwrap();
        assert_eq!(again.stacks.len(), original.stacks.len());
        assert_eq!(again.stacks[0].frames[0].runtime, Runtime::Browser);
        assert_eq!(again.stacks[0].frames[0].name, "work");
        assert_eq!(again.stacks[0].frames[0].line, Some(10));
        assert_eq!(again.total_weight(), original.total_weight());
    }

    #[test]
    fn raw_cpuprofile_is_written_unchanged() {
        let mut profile = import_cpuprofile(sample().as_bytes(), Runtime::Node).unwrap();
        let raw =
            br#"{"nodes":[{"id":1}],"samples":[1],"timeDeltas":[5],"startTime":9,"endTime":14}"#;
        profile.raw_cpuprofile = Some(raw.to_vec());
        let bytes = export(&profile, Format::Cpuprofile).unwrap();
        assert_eq!(bytes, raw);
    }

    #[test]
    fn hit_counts_are_used_when_there_are_no_samples() {
        let raw = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 0, "children": [2]},
            {"id": 2, "callFrame": {"functionName": "only", "url": "a.js", "lineNumber": 2, "columnNumber": -1}, "hitCount": 4, "children": []}
          ]
        }"#;
        let profile = import_cpuprofile(raw.as_bytes(), Runtime::Node).unwrap();
        assert_eq!(profile.stacks.len(), 1);
        assert_eq!(profile.stacks[0].weight, 4);
        assert_eq!(profile.stacks[0].frames[0].name, "only");
        assert_eq!(profile.stacks[0].frames[0].line, Some(3));
        assert_eq!(profile.stacks[0].frames[0].column, None);
    }

    #[test]
    fn negative_time_deltas_keep_later_samples_aligned() {
        let raw = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": [2, 3]},
            {"id": 2, "callFrame": {"functionName": "a", "url": "", "lineNumber": -1, "columnNumber": -1}},
            {"id": 3, "callFrame": {"functionName": "b", "url": "", "lineNumber": -1, "columnNumber": -1}}
          ],
          "samples": [2, 2, 3],
          "timeDeltas": [100, -5, 300]
        }"#;
        let profile = import_cpuprofile(raw.as_bytes(), Runtime::Node).unwrap();
        let weight = |name: &str| {
            profile
                .stacks
                .iter()
                .find(|stack| stack.frames[0].name == name)
                .unwrap()
                .weight
        };
        assert_eq!(weight("a"), 101);
        assert_eq!(weight("b"), 300);
    }

    #[test]
    fn sample_counts_count_v8_samples_not_microseconds() {
        let raw = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": [2, 3, 4]},
            {"id": 2, "callFrame": {"functionName": "(idle)", "url": "", "lineNumber": -1, "columnNumber": -1}},
            {"id": 3, "callFrame": {"functionName": "(program)", "url": "", "lineNumber": -1, "columnNumber": -1}},
            {"id": 4, "callFrame": {"functionName": "handle", "url": "a.js", "lineNumber": 1, "columnNumber": 1}}
          ],
          "samples": [2, 2, 3, 4, 2],
          "timeDeltas": [125, 125, 125, 125, 125]
        }"#;
        let counts = super::sample_counts(raw.as_bytes()).unwrap();
        assert_eq!(counts.total, 5);
        assert_eq!(counts.idle, 4);

        let wrapped = format!(r#"{{"profile": {raw}}}"#);
        assert_eq!(super::sample_counts(wrapped.as_bytes()).unwrap(), counts);

        let hits = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(idle)", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 7},
            {"id": 2, "callFrame": {"functionName": "work", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 3}
          ]
        }"#;
        let counts = super::sample_counts(hits.as_bytes()).unwrap();
        assert_eq!((counts.total, counts.idle), (10, 7));
    }

    #[test]
    fn explicit_runtime_prefix_is_kept() {
        let raw = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "[native] main", "url": "", "lineNumber": -1, "columnNumber": -1}, "hitCount": 2, "children": []}
          ]
        }"#;
        let profile = import_cpuprofile(raw.as_bytes(), Runtime::Node).unwrap();
        assert_eq!(profile.stacks[0].frames[0].runtime, Runtime::Native);
        assert_eq!(profile.stacks[0].frames[0].name, "main");
        assert_eq!(profile.stacks[0].weight, 2);
    }
}
