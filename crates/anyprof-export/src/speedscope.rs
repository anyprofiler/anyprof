use std::collections::HashMap;
use std::time::Duration;

use anyprof_core::{Frame, Profile, Stack, TimedSample};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::pprof::{
    format_thread_label, parse_tid_from_thread_label, ticks_from_f64, time_scale_ns,
};
use crate::symbol::{format_symbol, parse_symbol};

const SCHEMA: &str = "https://www.speedscope.app/file-format-schema.json";

pub(crate) fn export(profile: &Profile) -> Result<Vec<u8>, Error> {
    let mut frames = Vec::new();
    let mut index = HashMap::new();
    let profiles = if profile.samples.is_empty() {
        let mut samples = Vec::new();
        let mut weights = Vec::new();
        let mut total = 0_u64;

        for stack in &profile.stacks {
            if stack.weight == 0 {
                continue;
            }
            let mut sample = Vec::with_capacity(stack.frames.len());
            // speedscope samples are root-first; the data model is leaf-first.
            for frame in stack.frames.iter().rev() {
                sample.push(intern_frame(&mut frames, &mut index, frame));
            }
            total = total.saturating_add(stack.weight);
            weights.push(stack.weight);
            samples.push(sample);
        }
        vec![SampledProfile {
            kind: "sampled",
            name: profile_label(profile),
            unit: "none",
            start_value: 0,
            end_value: total,
            samples,
            weights,
        }]
    } else {
        export_timed_profiles(profile, &mut frames, &mut index)
    };

    let file = SpeedFile {
        schema: SCHEMA,
        exporter: concat!("anyprof@", env!("CARGO_PKG_VERSION")),
        name: profile_label(profile),
        shared: Shared { frames },
        profiles,
    };
    let mut bytes = serde_json::to_vec_pretty(&file)
        .map_err(|err| Error::invalid("speedscope", format!("failed to encode JSON: {err}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn export_timed_profiles(
    profile: &Profile,
    frames: &mut Vec<SpeedFrame>,
    index: &mut HashMap<FrameKey, usize>,
) -> Vec<SampledProfile> {
    let step_ns = profile
        .period
        .and_then(|p| u64::try_from(p.as_nanos()).ok())
        .filter(|p| *p > 0)
        .unwrap_or(10_000_000);
    let wall_ns = u64::try_from(profile.duration.as_nanos()).unwrap_or(0);

    let mut stack_ids: Vec<Vec<usize>> = Vec::with_capacity(profile.stacks.len());
    for stack in &profile.stacks {
        let mut ids = Vec::with_capacity(stack.frames.len());
        for frame in stack.frames.iter().rev() {
            ids.push(intern_frame(frames, index, frame));
        }
        stack_ids.push(ids);
    }

    let mut track_order: Vec<String> = Vec::new();
    let mut by_track: HashMap<String, Vec<(u64, usize, u64)>> = HashMap::new();
    for timed in &profile.samples {
        if timed.weight == 0 {
            continue;
        }
        let Some(ids) = stack_ids
            .get(timed.stack_index)
            .filter(|ids| !ids.is_empty())
        else {
            continue;
        };
        let _ = ids;
        let label = format_thread_label(timed.tid, timed.thread_name.as_deref())
            .unwrap_or_else(|| profile_label(profile));
        if !by_track.contains_key(&label) {
            track_order.push(label.clone());
        }
        by_track
            .entry(label)
            .or_default()
            .push((timed.offset_ns, timed.stack_index, timed.weight));
    }

    let mut out = Vec::with_capacity(track_order.len().max(1));
    for label in track_order {
        let Some(mut entries) = by_track.remove(&label) else {
            continue;
        };
        entries.sort_by_key(|(offset_ns, _, _)| *offset_ns);
        let mut samples = Vec::with_capacity(entries.len() * 2);
        let mut weights = Vec::with_capacity(entries.len() * 2);
        let mut cursor = 0_u64;
        for i in 0..entries.len() {
            let (offset_ns, stack_index, count) = entries[i];
            if offset_ns > cursor {
                samples.push(Vec::new());
                weights.push(offset_ns - cursor);
                cursor = offset_ns;
            }
            let nominal = step_ns.saturating_mul(count).max(1);
            let dur = if let Some((next_offset, _, _)) = entries.get(i + 1) {
                if *next_offset > cursor {
                    nominal.min(*next_offset - cursor)
                } else {
                    1
                }
            } else {
                nominal
            };
            samples.push(stack_ids[stack_index].clone());
            weights.push(dur);
            cursor = cursor.saturating_add(dur);
        }
        out.push(SampledProfile {
            kind: "sampled",
            name: label,
            unit: "nanoseconds",
            start_value: 0,
            end_value: wall_ns.max(cursor),
            samples,
            weights,
        });
    }
    if out.is_empty() {
        out.push(SampledProfile {
            kind: "sampled",
            name: profile_label(profile),
            unit: "nanoseconds",
            start_value: 0,
            end_value: wall_ns,
            samples: Vec::new(),
            weights: Vec::new(),
        });
    }
    out
}

pub(crate) fn import(bytes: &[u8]) -> Result<Profile, Error> {
    let file: SpeedFileIn = serde_json::from_slice(bytes)
        .map_err(|err| Error::invalid("speedscope", format!("invalid JSON: {err}")))?;
    let sampled: Vec<SampledIn> = file
        .profiles
        .into_iter()
        .filter(|profile| profile.kind == "sampled")
        .collect();
    if sampled.is_empty() {
        return Err(Error::invalid(
            "speedscope",
            "only sampled speedscope profiles can be imported",
        ));
    }

    let first_unit = sampled[0].unit.as_deref().unwrap_or("none");
    let anyprof_timed = file
        .exporter
        .as_deref()
        .is_some_and(|exp| exp.starts_with("anyprof@"))
        && time_scale_ns(first_unit).is_some();

    if sampled.len() == 1 && !anyprof_timed {
        let profile = sampled.into_iter().next().unwrap();
        let samples = profile.samples.unwrap_or_default();
        let weights = profile.weights.unwrap_or_default();
        if samples.len() != weights.len() {
            return Err(Error::invalid(
                "speedscope",
                format!(
                    "samples ({}) and weights ({}) have different lengths",
                    samples.len(),
                    weights.len()
                ),
            ));
        }

        let mut stacks = Vec::with_capacity(samples.len());
        for (sample, weight) in samples.into_iter().zip(weights) {
            let weight = json_weight(&weight)?;
            if sample.is_empty() || weight == 0 {
                continue;
            }
            let mut frames = Vec::with_capacity(sample.len());
            for frame_index in sample.iter().rev() {
                let frame = file.shared.frames.get(*frame_index).ok_or_else(|| {
                    Error::invalid(
                        "speedscope",
                        format!("frame index {frame_index} is out of range"),
                    )
                })?;
                frames.push(frame_from_speed(frame));
            }
            stacks.push(Stack { frames, weight });
        }

        return Ok(Profile {
            comm: profile.name,
            stacks,
            ..Profile::default()
        });
    }

    // Multi-profile or anyprof timed speedscope: fold unique stacks and recover timed samples when on a time unit.
    let mut unique: Vec<(Vec<Frame>, u64)> = Vec::new();
    let mut stack_map: HashMap<Vec<Frame>, usize> = HashMap::new();
    let mut raw_timed: Vec<(u64, Option<String>, u64, usize, u64)> = Vec::new();
    let mut max_end_ns = 0_u64;

    for (idx, prof) in sampled.iter().enumerate() {
        let unit = prof.unit.as_deref().unwrap_or("none");
        let scale = time_scale_ns(unit);
        let samples = prof.samples.as_deref().unwrap_or(&[]);
        let weights = prof.weights.as_deref().unwrap_or(&[]);
        if samples.len() != weights.len() {
            return Err(Error::invalid(
                "speedscope",
                format!(
                    "profile {idx} samples ({}) and weights ({}) have different lengths",
                    samples.len(),
                    weights.len()
                ),
            ));
        }
        let mut cursor_ns = match (prof.start_value, scale) {
            (Some(start), Some(sc)) if start >= 0.0 => ticks_from_f64(start * sc, "speedscope")?,
            _ => 0,
        };
        if let (Some(end), Some(sc)) = (prof.end_value, scale)
            && end >= 0.0
        {
            max_end_ns = max_end_ns.max(ticks_from_f64(end * sc, "speedscope")?);
        }
        let label = prof.name.as_deref().unwrap_or("");
        let tid = parse_tid_from_thread_label(label).unwrap_or(0);
        let thread_name = if label.is_empty() || (tid > 0 && label == format!("Thread {tid}")) {
            None
        } else if tid > 0 {
            let suffix = format!(" ({tid})");
            let trimmed = label.strip_suffix(&suffix).unwrap_or(label).trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        } else {
            Some(label.to_string())
        };

        for (sample, raw_weight) in samples.iter().zip(weights) {
            let w_raw = json_f64_nonneg(raw_weight)?;
            let step_ticks = match scale {
                Some(sc) => ticks_from_f64(w_raw * sc, "speedscope")?,
                None => w_raw.round() as u64,
            };
            if sample.is_empty() {
                cursor_ns = cursor_ns.saturating_add(step_ticks);
                continue;
            }
            let count_weight = if anyprof_timed { 1 } else { step_ticks };
            if count_weight == 0 {
                cursor_ns = cursor_ns.saturating_add(step_ticks);
                continue;
            }
            let mut frames = Vec::with_capacity(sample.len());
            for frame_index in sample.iter().rev() {
                let frame = file.shared.frames.get(*frame_index).ok_or_else(|| {
                    Error::invalid(
                        "speedscope",
                        format!("frame index {frame_index} is out of range"),
                    )
                })?;
                frames.push(frame_from_speed(frame));
            }
            let uid = if let Some(&id) = stack_map.get(&frames) {
                unique[id].1 = unique[id].1.saturating_add(count_weight);
                id
            } else {
                let id = unique.len();
                stack_map.insert(frames.clone(), id);
                unique.push((frames, count_weight));
                id
            };
            if scale.is_some() {
                raw_timed.push((tid, thread_name.clone(), cursor_ns, uid, count_weight));
            }
            cursor_ns = cursor_ns.saturating_add(step_ticks);
        }
        max_end_ns = max_end_ns.max(cursor_ns);
    }

    let count = unique.len();
    let mut indexed: Vec<(usize, Stack)> = unique
        .into_iter()
        .enumerate()
        .map(|(uid, (frames, weight))| (uid, Stack { frames, weight }))
        .collect();
    indexed.sort_by_key(|(_, stack)| std::cmp::Reverse(stack.weight));
    let mut remap = vec![0usize; count];
    let mut stacks = Vec::with_capacity(count);
    for (new_idx, (old_uid, stack)) in indexed.into_iter().enumerate() {
        remap[old_uid] = new_idx;
        stacks.push(stack);
    }
    raw_timed.sort_by_key(|(_, _, offset_ns, _, _)| *offset_ns);
    let timed_samples: Vec<TimedSample> = raw_timed
        .into_iter()
        .map(|(tid, thread_name, offset_ns, uid, weight)| TimedSample {
            tid,
            thread_name,
            offset_ns,
            stack_index: remap[uid],
            weight,
        })
        .collect();

    Ok(Profile {
        comm: file.name.or_else(|| sampled[0].name.clone()),
        duration: if max_end_ns > 0 {
            Duration::from_nanos(max_end_ns)
        } else {
            Duration::ZERO
        },
        stacks,
        samples: timed_samples,
        ..Profile::default()
    })
}

fn profile_label(profile: &Profile) -> String {
    match (&profile.comm, profile.pid) {
        (Some(comm), 0) => comm.clone(),
        (Some(comm), pid) => format!("{comm} ({pid})"),
        (None, 0) => "anyprof".to_string(),
        (None, pid) => format!("pid {pid}"),
    }
}

fn intern_frame(
    frames: &mut Vec<SpeedFrame>,
    index: &mut HashMap<FrameKey, usize>,
    frame: &Frame,
) -> usize {
    let key = FrameKey {
        name: format_symbol(frame),
        file: frame.file.clone(),
        line: frame.line,
        column: frame.column,
        module: frame.module.clone(),
    };
    if let Some(existing) = index.get(&key) {
        return *existing;
    }
    let id = frames.len();
    frames.push(SpeedFrame {
        name: key.name.clone(),
        file: key.file.clone(),
        line: key.line,
        col: key.column,
        module: key.module.clone(),
    });
    index.insert(key, id);
    id
}

fn frame_from_speed(frame: &SpeedFrame) -> Frame {
    let (runtime, name) = parse_symbol(&frame.name);
    Frame {
        runtime,
        name: name.to_string(),
        file: frame.file.clone(),
        line: frame.line,
        column: frame.col,
        module: frame.module.clone(),
        address: None,
    }
}

fn json_weight(value: &serde_json::Value) -> Result<u64, Error> {
    Ok(json_f64_nonneg(value)?.round() as u64)
}

fn json_f64_nonneg(value: &serde_json::Value) -> Result<f64, Error> {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(weight) = number.as_u64() {
                return Ok(weight as f64);
            }
            if let Some(weight) = number.as_f64()
                && weight.is_finite()
                && weight >= 0.0
            {
                return Ok(weight);
            }
            Err(Error::invalid(
                "speedscope",
                format!("invalid weight {number}"),
            ))
        }
        other => Err(Error::invalid(
            "speedscope",
            format!("weight must be a number, got {other}"),
        )),
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FrameKey {
    name: String,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    module: Option<String>,
}

#[derive(Serialize)]
struct SpeedFile {
    #[serde(rename = "$schema")]
    schema: &'static str,
    exporter: &'static str,
    name: String,
    shared: Shared,
    profiles: Vec<SampledProfile>,
}

#[derive(Serialize)]
struct Shared {
    frames: Vec<SpeedFrame>,
}

#[derive(Serialize)]
struct SampledProfile {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    unit: &'static str,
    #[serde(rename = "startValue")]
    start_value: u64,
    #[serde(rename = "endValue")]
    end_value: u64,
    samples: Vec<Vec<usize>>,
    weights: Vec<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SpeedFrame {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    module: Option<String>,
}

#[derive(Deserialize)]
struct SpeedFileIn {
    #[serde(default)]
    exporter: Option<String>,
    #[serde(default)]
    name: Option<String>,
    shared: SharedIn,
    profiles: Vec<SampledIn>,
}

#[derive(Deserialize)]
struct SharedIn {
    frames: Vec<SpeedFrame>,
}

#[derive(Deserialize)]
struct SampledIn {
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    unit: Option<String>,
    #[serde(rename = "startValue")]
    start_value: Option<f64>,
    #[serde(rename = "endValue")]
    end_value: Option<f64>,
    samples: Option<Vec<Vec<usize>>>,
    weights: Option<Vec<serde_json::Value>>,
}

#[cfg(test)]
mod tests {
    use super::export;
    use crate::{Format, import};
    use anyprof_core::{Frame, Profile, Runtime, Stack, TimedSample};
    use std::time::Duration;

    #[test]
    fn roundtrip_preserves_frames_files_and_weights() {
        let original = crate::fixture();
        let bytes = export(&original).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains(super::SCHEMA), "{text}");
        assert!(text.contains("anyprof@"), "{text}");

        let profile = import(&bytes, Format::Speedscope).unwrap();
        assert_eq!(profile.stacks.len(), original.stacks.len());
        assert_eq!(profile.total_weight(), original.total_weight());
        assert_eq!(profile.comm.as_deref(), Some("demo (4242)"));

        let leaf = &profile.stacks[0].frames[0];
        assert_eq!(leaf.runtime, Runtime::Node);
        assert_eq!(leaf.name, "work");
        assert_eq!(leaf.file.as_deref(), Some("app.js"));
        assert_eq!(leaf.line, Some(12));
        assert_eq!(leaf.column, Some(3));
        assert_eq!(profile.stacks[0].frames[1].name, "main");
        assert_eq!(profile.stacks[0].frames[1].runtime, Runtime::Native);
        assert_eq!(profile.stacks[0].weight, 2);

        assert_eq!(profile.stacks[2].frames[0].runtime, Runtime::Go);
        assert_eq!(profile.stacks[2].frames[1].runtime, Runtime::Kernel);
        assert_eq!(profile.stacks[2].weight, 4);
    }

    #[test]
    fn timed_multi_thread_roundtrip_preserves_tracks_and_modules() {
        let mut f_py = Frame::new(Runtime::Python, "compute");
        f_py.file = Some("worker.py".to_string());
        f_py.line = Some(19);
        let mut f_libc = Frame::new(Runtime::Native, "poll");
        f_libc.module = Some("libc.so.6".to_string());

        let original = Profile {
            pid: 77,
            comm: Some("python3".to_string()),
            duration: Duration::from_millis(80),
            period: Some(Duration::from_millis(10)),
            stacks: vec![
                Stack {
                    frames: vec![f_py],
                    weight: 2,
                },
                Stack {
                    frames: vec![f_libc],
                    weight: 1,
                },
            ],
            samples: vec![
                TimedSample {
                    tid: 201,
                    thread_name: Some("worker-a".to_string()),
                    offset_ns: 10_000_000,
                    stack_index: 0,
                    weight: 1,
                },
                TimedSample {
                    tid: 202,
                    thread_name: Some("worker-b".to_string()),
                    offset_ns: 25_000_000,
                    stack_index: 1,
                    weight: 1,
                },
                TimedSample {
                    tid: 201,
                    thread_name: Some("worker-a".to_string()),
                    offset_ns: 40_000_000,
                    stack_index: 0,
                    weight: 1,
                },
            ],
            raw_cpuprofile: None,
        };
        let bytes = export(&original).unwrap();
        let decoded = import(&bytes, Format::Speedscope).unwrap();
        assert_eq!(decoded.total_weight(), 3);
        assert_eq!(decoded.samples.len(), 3);
        assert_eq!(decoded.samples[0].tid, 201);
        assert_eq!(decoded.samples[0].thread_name.as_deref(), Some("worker-a"));
        assert_eq!(decoded.samples[0].offset_ns, 10_000_000);
        assert_eq!(decoded.samples[1].tid, 202);
        assert_eq!(decoded.samples[1].offset_ns, 25_000_000);
        let poll_stack = decoded
            .stacks
            .iter()
            .find(|s| s.frames[0].name == "poll")
            .unwrap();
        assert_eq!(poll_stack.frames[0].module.as_deref(), Some("libc.so.6"));
    }

    #[test]
    fn rejects_evented_profiles() {
        let json = br#"{
            "shared": {"frames": []},
            "profiles": [{"type": "evented", "name": "x", "unit": "none", "startValue": 0, "endValue": 1, "events": []}]
        }"#;
        let err = import(json, Format::Speedscope).unwrap_err();
        assert!(err.to_string().contains("sampled"), "{err}");
    }
}
