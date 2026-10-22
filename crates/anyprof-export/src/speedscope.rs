use std::collections::HashMap;

use anyprof_core::{Frame, Profile, Stack};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::symbol::{format_symbol, parse_symbol};

const SCHEMA: &str = "https://www.speedscope.app/file-format-schema.json";

pub(crate) fn export(profile: &Profile) -> Result<Vec<u8>, Error> {
    let mut frames = Vec::new();
    let mut index = HashMap::new();
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

    let file = SpeedFile {
        schema: SCHEMA,
        exporter: concat!("anyprof@", env!("CARGO_PKG_VERSION")),
        name: profile_label(profile),
        shared: Shared { frames },
        profiles: vec![SampledProfile {
            kind: "sampled",
            name: profile_label(profile),
            unit: "none",
            start_value: 0,
            end_value: total,
            samples,
            weights,
        }],
    };
    let mut bytes = serde_json::to_vec_pretty(&file)
        .map_err(|err| Error::invalid("speedscope", format!("failed to encode JSON: {err}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn import(bytes: &[u8]) -> Result<Profile, Error> {
    let file: SpeedFileIn = serde_json::from_slice(bytes)
        .map_err(|err| Error::invalid("speedscope", format!("invalid JSON: {err}")))?;
    let profile = file
        .profiles
        .into_iter()
        .find(|profile| profile.kind == "sampled")
        .ok_or_else(|| {
            Error::invalid(
                "speedscope",
                "only sampled speedscope profiles can be imported",
            )
        })?;
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

    Ok(Profile {
        comm: profile.name,
        stacks,
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
        module: None,
        address: None,
    }
}

fn json_weight(value: &serde_json::Value) -> Result<u64, Error> {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(weight) = number.as_u64() {
                return Ok(weight);
            }
            if let Some(weight) = number.as_f64() {
                if weight.is_finite() && weight >= 0.0 {
                    return Ok(weight.round() as u64);
                }
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
}

#[derive(Deserialize)]
struct SpeedFileIn {
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
    samples: Option<Vec<Vec<usize>>>,
    weights: Option<Vec<serde_json::Value>>,
}

#[cfg(test)]
mod tests {
    use super::export;
    use crate::{Format, import};
    use anyprof_core::Runtime;

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
    fn rejects_evented_profiles() {
        let json = br#"{
            "shared": {"frames": []},
            "profiles": [{"type": "evented", "name": "x", "unit": "none", "startValue": 0, "endValue": 1, "events": []}]
        }"#;
        let err = import(json, Format::Speedscope).unwrap_err();
        assert!(err.to_string().contains("sampled"), "{err}");
    }
}
