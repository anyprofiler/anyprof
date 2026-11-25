//! gzip-compressed pprof profiles (`perftools.profiles.Profile`).
//!
//! Writes sample type `samples/count`, mappings from `Frame::module`, one
//! location per frame, function name/file/line, optional address, duration, and
//! period (in nanoseconds). When [`Profile::samples`] is populated, each sample
//! carries `thread` and `offset_ns` labels so thread tracks and the timeline
//! survive a roundtrip.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use anyprof_core::{Frame, Profile, Runtime, Stack, TimedSample};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::error::Error;
use crate::proto::{Reader, Value, Writer, decode_varints};
use crate::symbol::{format_symbol, parse_symbol};

pub(crate) fn export(profile: &Profile) -> Result<Vec<u8>, Error> {
    let mut strings = StringTable::new();
    let samples_label = strings.intern("samples");
    let count_label = strings.intern("count");
    let cpu_label = strings.intern("cpu");
    let nanos_label = strings.intern("nanoseconds");

    let mut mappings: HashMap<u64, u64> = HashMap::new();
    let mut mapping_msgs = Vec::new();
    let mut functions: HashMap<(u64, u64, i64), u64> = HashMap::new();
    let mut function_msgs = Vec::new();
    let mut locations: HashMap<(u64, u64, i64, i64, u64), u64> = HashMap::new();
    let mut location_msgs = Vec::new();
    let mut sample_msgs = Vec::new();

    if profile.samples.is_empty() {
        for stack in &profile.stacks {
            if stack.weight == 0 {
                continue;
            }
            if stack.weight > i64::MAX as u64 {
                return Err(Error::invalid(
                    "pprof",
                    format!("sample weight {} does not fit in i64", stack.weight),
                ));
            }
            let mut location_ids = Vec::with_capacity(stack.frames.len());
            for frame in &stack.frames {
                location_ids.push(intern_location(
                    frame,
                    &mut strings,
                    &mut mappings,
                    &mut mapping_msgs,
                    &mut functions,
                    &mut function_msgs,
                    &mut locations,
                    &mut location_msgs,
                ));
            }
            let mut sample = Writer::new();
            sample.field_packed_varints(1, &location_ids);
            sample.field_packed_varints(2, &[stack.weight]);
            sample_msgs.push(sample);
        }
    } else {
        let thread_key = strings.intern("thread");
        let offset_key = strings.intern("offset_ns");
        let mut stack_locations = Vec::with_capacity(profile.stacks.len());
        for stack in &profile.stacks {
            let mut ids = Vec::with_capacity(stack.frames.len());
            for frame in &stack.frames {
                ids.push(intern_location(
                    frame,
                    &mut strings,
                    &mut mappings,
                    &mut mapping_msgs,
                    &mut functions,
                    &mut function_msgs,
                    &mut locations,
                    &mut location_msgs,
                ));
            }
            stack_locations.push(ids);
        }
        for timed in &profile.samples {
            if timed.weight == 0 {
                continue;
            }
            if timed.weight > i64::MAX as u64 {
                return Err(Error::invalid(
                    "pprof",
                    format!("sample weight {} does not fit in i64", timed.weight),
                ));
            }
            let Some(location_ids) = stack_locations
                .get(timed.stack_index)
                .filter(|ids| !ids.is_empty())
            else {
                continue;
            };
            let mut sample = Writer::new();
            sample.field_packed_varints(1, location_ids);
            sample.field_packed_varints(2, &[timed.weight]);
            if let Some(label_text) = format_thread_label(timed.tid, timed.thread_name.as_deref()) {
                let mut tlabel = Writer::new();
                tlabel.field_varint(1, thread_key);
                tlabel.field_varint(2, strings.intern(&label_text));
                if timed.tid > 0 && timed.tid <= i64::MAX as u64 {
                    tlabel.field_varint(3, timed.tid);
                }
                sample.field_message(3, &tlabel);
            }
            let mut olabel = Writer::new();
            olabel.field_varint(1, offset_key);
            olabel.field_varint(3, timed.offset_ns.min(i64::MAX as u64));
            olabel.field_varint(4, nanos_label);
            sample.field_message(3, &olabel);
            sample_msgs.push(sample);
        }
    }

    let mut message = Writer::new();
    message.field_message(1, &value_type(samples_label, count_label));
    for sample in &sample_msgs {
        message.field_message(2, sample);
    }
    for mapping in &mapping_msgs {
        message.field_message(3, mapping);
    }
    for location in &location_msgs {
        message.field_message(4, location);
    }
    for function in &function_msgs {
        message.field_message(5, function);
    }
    for value in strings.values() {
        message.field_string(6, value);
    }
    if let Some(duration) = duration_nanos(profile.duration)? {
        message.field_varint(10, duration);
    }
    if let Some(period) = profile.period
        && let Some(period) = duration_nanos(period)?
    {
        message.field_message(11, &value_type(cpu_label, nanos_label));
        message.field_varint(12, period);
    }
    gzip(&message.into_vec())
}

pub(crate) fn format_thread_label(tid: u64, name: Option<&str>) -> Option<String> {
    let clean = name.map(str::trim).filter(|n| !n.is_empty());
    match (clean, tid) {
        (Some(name), 0) => Some(name.to_string()),
        (Some(name), tid) => Some(format!("{name} ({tid})")),
        (None, 0) => None,
        (None, tid) => Some(format!("Thread {tid}")),
    }
}

pub(crate) fn import(bytes: &[u8]) -> Result<Profile, Error> {
    let decoded = gunzip_if_needed(bytes)?;
    let mut raw = parse_profile(&decoded)?;
    if raw.strings.first().is_some_and(|first| !first.is_empty()) {
        return Err(Error::invalid("pprof", "string_table[0] must be empty"));
    }

    let raw_samples = std::mem::take(&mut raw.samples);
    let mut has_timed = false;
    for sample in &raw_samples {
        if offset_from_labels(&raw, &sample.labels)?.is_some() {
            has_timed = true;
            break;
        }
    }

    let mut stacks = Vec::new();
    let mut timed_out = Vec::new();

    if has_timed {
        let mut unique: Vec<(Vec<Frame>, u64)> = Vec::new();
        let mut index: HashMap<Vec<Frame>, usize> = HashMap::new();
        let mut raw_timed: Vec<(u64, Option<String>, u64, usize, u64)> = Vec::new();
        for sample in raw_samples {
            let weight = sample_first_weight(&sample)?;
            let frames = decode_sample_frames(&raw, &sample.locations)?;
            if weight == 0 || frames.is_empty() {
                continue;
            }
            let uid = if let Some(&id) = index.get(&frames) {
                unique[id].1 = unique[id].1.saturating_add(weight);
                id
            } else {
                let id = unique.len();
                index.insert(frames.clone(), id);
                unique.push((frames, weight));
                id
            };
            if let Some(offset_ns) = offset_from_labels(&raw, &sample.labels)? {
                let (tid, thread_name) = thread_id_and_name(&raw, &sample.labels)?;
                raw_timed.push((tid, thread_name, offset_ns, uid, weight));
            }
        }
        let count = unique.len();
        let mut indexed: Vec<(usize, Stack)> = unique
            .into_iter()
            .enumerate()
            .map(|(uid, (frames, weight))| (uid, Stack { frames, weight }))
            .collect();
        indexed.sort_by_key(|(_, stack)| std::cmp::Reverse(stack.weight));
        let mut remap = vec![0usize; count];
        for (new_idx, (old_uid, stack)) in indexed.into_iter().enumerate() {
            remap[old_uid] = new_idx;
            stacks.push(stack);
        }
        raw_timed.sort_by_key(|(_, _, offset_ns, _, _)| *offset_ns);
        for (tid, thread_name, offset_ns, uid, weight) in raw_timed {
            timed_out.push(TimedSample {
                tid,
                thread_name,
                offset_ns,
                stack_index: remap[uid],
                weight,
            });
        }
    } else {
        stacks.reserve(raw_samples.len());
        for sample in raw_samples {
            let weight = sample_first_weight(&sample)?;
            let frames = decode_sample_frames(&raw, &sample.locations)?;
            stacks.push(Stack { frames, weight });
        }
    }

    let period = if raw.period_nanos > 0 && period_is_nanos(&raw)? {
        Some(Duration::from_nanos(
            u64::try_from(raw.period_nanos).unwrap_or(0),
        ))
    } else {
        None
    };
    let duration = if raw.duration_nanos > 0 {
        Duration::from_nanos(u64::try_from(raw.duration_nanos).unwrap_or(0))
    } else {
        Duration::ZERO
    };

    Ok(Profile {
        duration,
        period,
        stacks,
        samples: timed_out,
        ..Profile::default()
    })
}

fn sample_first_weight(sample: &RawSample) -> Result<u64, Error> {
    match sample.values.first() {
        Some(value) if *value >= 0 => Ok(u64::try_from(*value).unwrap_or(0)),
        Some(value) => Err(Error::invalid(
            "pprof",
            format!("negative sample value {value}"),
        )),
        None => Ok(0),
    }
}

fn decode_sample_frames(raw: &RawProfile, location_ids: &[u64]) -> Result<Vec<Frame>, Error> {
    let mut frames = Vec::new();
    for location_id in location_ids {
        let location = raw
            .locations
            .get(location_id)
            .ok_or_else(|| Error::invalid("pprof", format!("unknown location id {location_id}")))?;
        let module = mapping_name(raw, location.mapping_id)?;
        if location.lines.is_empty() {
            frames.push(Frame {
                address: none_if_zero(location.address),
                module,
                ..Frame::new(Runtime::Native, "")
            });
            continue;
        }
        for line in &location.lines {
            let mut frame = frame_from_line(raw, location.address, line)?;
            frame.module = module.clone();
            frames.push(frame);
        }
    }
    Ok(frames)
}

#[allow(clippy::too_many_arguments)]
fn intern_location(
    frame: &Frame,
    strings: &mut StringTable,
    mappings: &mut HashMap<u64, u64>,
    mapping_msgs: &mut Vec<Writer>,
    functions: &mut HashMap<(u64, u64, i64), u64>,
    function_msgs: &mut Vec<Writer>,
    locations: &mut HashMap<(u64, u64, i64, i64, u64), u64>,
    location_msgs: &mut Vec<Writer>,
) -> u64 {
    let mapping_id = match frame.module.as_deref().filter(|m| !m.is_empty()) {
        Some(module_name) => {
            let file_idx = strings.intern(module_name);
            *mappings.entry(file_idx).or_insert_with(|| {
                let id = mapping_msgs.len() as u64 + 1;
                let mut msg = Writer::new();
                msg.field_varint(1, id);
                msg.field_varint(5, file_idx);
                mapping_msgs.push(msg);
                id
            })
        }
        None => 0,
    };

    let name = strings.intern(&format_symbol(frame));
    let filename = strings.intern(frame.file.as_deref().unwrap_or(""));
    let start_line = i64::from(frame.line.unwrap_or(0));
    let column = i64::from(frame.column.unwrap_or(0));
    let address = frame.address.unwrap_or(0);
    let function_key = (name, filename, start_line);
    let function_id = *functions.entry(function_key).or_insert_with(|| {
        let id = function_msgs.len() as u64 + 1;
        let mut message = Writer::new();
        message.field_varint(1, id);
        message.field_varint(2, name);
        message.field_varint(3, name);
        message.field_varint(4, filename);
        if start_line > 0 {
            message.field_varint(5, start_line as u64);
        }
        function_msgs.push(message);
        id
    });

    let location_key = (mapping_id, function_id, start_line, column, address);
    *locations.entry(location_key).or_insert_with(|| {
        let id = location_msgs.len() as u64 + 1;
        let mut line = Writer::new();
        line.field_varint(1, function_id);
        if start_line > 0 {
            line.field_varint(2, start_line as u64);
        }
        if column > 0 {
            line.field_varint(3, column as u64);
        }
        let mut message = Writer::new();
        message.field_varint(1, id);
        if mapping_id > 0 {
            message.field_varint(2, mapping_id);
        }
        message.field_varint(3, address);
        message.field_message(4, &line);
        location_msgs.push(message);
        id
    })
}

fn value_type(type_index: u64, unit_index: u64) -> Writer {
    let mut message = Writer::new();
    message.field_varint(1, type_index);
    message.field_varint(2, unit_index);
    message
}

fn duration_nanos(duration: Duration) -> Result<Option<u64>, Error> {
    if duration.is_zero() {
        return Ok(None);
    }
    let nanos = duration.as_nanos();
    let value = u64::try_from(nanos).map_err(|_| {
        Error::invalid(
            "pprof",
            format!("duration {nanos}ns does not fit in a protobuf int64"),
        )
    })?;
    if value > i64::MAX as u64 {
        return Err(Error::invalid(
            "pprof",
            format!("duration {nanos}ns does not fit in a protobuf int64"),
        ));
    }
    Ok(Some(value))
}

fn frame_from_line(raw: &RawProfile, address: u64, line: &RawLine) -> Result<Frame, Error> {
    let function = raw.functions.get(&line.function_id).ok_or_else(|| {
        Error::invalid("pprof", format!("unknown function id {}", line.function_id))
    })?;
    let symbol = lookup(&raw.strings, function.name)?;
    let (runtime, name) = parse_symbol(symbol);
    let file = {
        let filename = lookup(&raw.strings, function.filename)?;
        if filename.is_empty() {
            None
        } else {
            Some(filename.to_string())
        }
    };
    Ok(Frame {
        runtime,
        name: name.to_string(),
        file,
        line: none_if_zero_i64(line.line),
        column: none_if_zero_i64(line.column),
        module: None,
        address: none_if_zero(address),
    })
}

fn period_is_nanos(raw: &RawProfile) -> Result<bool, Error> {
    if raw.period_unit == 0 {
        return Ok(true);
    }
    Ok(lookup(&raw.strings, raw.period_unit)? == "nanoseconds")
}

fn lookup(strings: &[String], index: i64) -> Result<&str, Error> {
    if index < 0 {
        return Err(Error::invalid(
            "pprof",
            format!("negative string index {index}"),
        ));
    }
    let index = usize::try_from(index).map_err(|_| {
        Error::invalid(
            "pprof",
            format!("string index {index} does not fit in usize"),
        )
    })?;
    strings
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| Error::invalid("pprof", format!("string index {index} is out of range")))
}

fn none_if_zero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

fn none_if_zero_i64(value: i64) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value != 0)
}

struct StringTable {
    index: HashMap<String, u64>,
    values: Vec<String>,
}

impl StringTable {
    fn new() -> Self {
        let mut table = Self {
            index: HashMap::new(),
            values: Vec::new(),
        };
        table.intern("");
        table
    }

    fn intern(&mut self, value: &str) -> u64 {
        if let Some(id) = self.index.get(value) {
            return *id;
        }
        let id = self.values.len() as u64;
        self.values.push(value.to_string());
        self.index.insert(value.to_string(), id);
        id
    }

    fn values(&self) -> &[String] {
        &self.values
    }
}

struct RawProfile {
    sample_types: Vec<(i64, i64)>,
    samples: Vec<RawSample>,
    mappings: HashMap<u64, i64>,
    locations: HashMap<u64, RawLocation>,
    functions: HashMap<u64, RawFunction>,
    strings: Vec<String>,
    duration_nanos: i64,
    period_nanos: i64,
    period_type: i64,
    period_unit: i64,
    default_sample_type: i64,
}

struct RawSample {
    locations: Vec<u64>,
    values: Vec<i64>,
    labels: Vec<RawLabel>,
}

struct RawLabel {
    key: i64,
    str_idx: i64,
    num: i64,
    num_unit: i64,
    has_num: bool,
}

struct RawLocation {
    mapping_id: u64,
    address: u64,
    lines: Vec<RawLine>,
}

struct RawLine {
    function_id: u64,
    line: i64,
    column: i64,
}

struct RawFunction {
    name: i64,
    filename: i64,
}

fn parse_profile(bytes: &[u8]) -> Result<RawProfile, Error> {
    let mut raw = RawProfile {
        sample_types: Vec::new(),
        samples: Vec::new(),
        mappings: HashMap::new(),
        locations: HashMap::new(),
        functions: HashMap::new(),
        strings: Vec::new(),
        duration_nanos: 0,
        period_nanos: 0,
        period_type: 0,
        period_unit: 0,
        default_sample_type: 0,
    };
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Bytes(sample_type)) => {
                raw.sample_types.push(parse_value_type(&sample_type)?)
            }
            (2, Owned::Bytes(sample)) => raw.samples.push(parse_sample(&sample)?),
            (3, Owned::Bytes(mapping)) => {
                let (id, filename) = parse_mapping(&mapping)?;
                if id != 0 {
                    raw.mappings.insert(id, filename);
                }
            }
            (4, Owned::Bytes(location)) => {
                let (id, parsed) = parse_location(&location)?;
                raw.locations.insert(id, parsed);
            }
            (5, Owned::Bytes(function)) => {
                let (id, parsed) = parse_function(&function)?;
                raw.functions.insert(id, parsed);
            }
            (6, Owned::Bytes(text)) => {
                let text = String::from_utf8(text).map_err(|err| {
                    Error::invalid("pprof", format!("string table is not utf-8: {err}"))
                })?;
                raw.strings.push(text);
            }
            (10, Owned::Varint(duration)) => raw.duration_nanos = as_i64(duration)?,
            (11, Owned::Bytes(period_type)) => {
                let (type_index, unit_index) = parse_value_type(&period_type)?;
                raw.period_type = type_index;
                raw.period_unit = unit_index;
            }
            (12, Owned::Varint(period)) => raw.period_nanos = as_i64(period)?,
            (14, Owned::Varint(default_type)) => {
                raw.default_sample_type = as_i64(default_type)?;
            }
            _ => {}
        }
    }
    Ok(raw)
}

fn parse_sample(bytes: &[u8]) -> Result<RawSample, Error> {
    let mut sample = RawSample {
        locations: Vec::new(),
        values: Vec::new(),
        labels: Vec::new(),
    };
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(id)) => sample.locations.push(id),
            (1, Owned::Bytes(packed)) => sample.locations.extend(packed_u64(&packed)?),
            (2, Owned::Varint(value)) => sample.values.push(as_i64(value)?),
            (2, Owned::Bytes(packed)) => {
                for value in packed_u64(&packed)? {
                    sample.values.push(as_i64(value)?);
                }
            }
            (3, Owned::Bytes(label)) => sample.labels.push(parse_label(&label)?),
            _ => {}
        }
    }
    Ok(sample)
}

fn parse_location(bytes: &[u8]) -> Result<(u64, RawLocation), Error> {
    let mut id = 0_u64;
    let mut mapping_id = 0_u64;
    let mut address = 0_u64;
    let mut lines = Vec::new();
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => id = value,
            (2, Owned::Varint(value)) => mapping_id = value,
            (3, Owned::Varint(value)) => address = value,
            (4, Owned::Bytes(line)) => lines.push(parse_line(&line)?),
            _ => {}
        }
    }
    if id == 0 {
        return Err(Error::invalid("pprof", "location id 0"));
    }
    Ok((
        id,
        RawLocation {
            mapping_id,
            address,
            lines,
        },
    ))
}

fn parse_mapping(bytes: &[u8]) -> Result<(u64, i64), Error> {
    let mut id = 0_u64;
    let mut filename = 0_i64;
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => id = value,
            (5, Owned::Varint(value)) => filename = as_i64(value)?,
            _ => {}
        }
    }
    Ok((id, filename))
}

fn parse_label(bytes: &[u8]) -> Result<RawLabel, Error> {
    let mut label = RawLabel {
        key: 0,
        str_idx: 0,
        num: 0,
        num_unit: 0,
        has_num: false,
    };
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => label.key = as_i64(value)?,
            (2, Owned::Varint(value)) => label.str_idx = as_i64(value)?,
            (3, Owned::Varint(value)) => {
                label.num = as_i64(value)?;
                label.has_num = true;
            }
            (4, Owned::Varint(value)) => label.num_unit = as_i64(value)?,
            _ => {}
        }
    }
    Ok(label)
}

fn parse_line(bytes: &[u8]) -> Result<RawLine, Error> {
    let mut line = RawLine {
        function_id: 0,
        line: 0,
        column: 0,
    };
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => line.function_id = value,
            (2, Owned::Varint(value)) => line.line = as_i64(value)?,
            (3, Owned::Varint(value)) => line.column = as_i64(value)?,
            _ => {}
        }
    }
    Ok(line)
}

fn parse_function(bytes: &[u8]) -> Result<(u64, RawFunction), Error> {
    let mut id = 0_u64;
    let mut name = 0_i64;
    let mut filename = 0_i64;
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => id = value,
            (2, Owned::Varint(value)) => name = as_i64(value)?,
            (4, Owned::Varint(value)) => filename = as_i64(value)?,
            _ => {}
        }
    }
    if id == 0 {
        return Err(Error::invalid("pprof", "function id 0"));
    }
    Ok((id, RawFunction { name, filename }))
}

fn parse_value_type(bytes: &[u8]) -> Result<(i64, i64), Error> {
    let mut type_index = 0_i64;
    let mut unit_index = 0_i64;
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => type_index = as_i64(value)?,
            (2, Owned::Varint(value)) => unit_index = as_i64(value)?,
            _ => {}
        }
    }
    Ok((type_index, unit_index))
}

enum Owned {
    Varint(u64),
    Bytes(Vec<u8>),
}

fn message_fields(bytes: &[u8]) -> Result<Vec<(u32, Owned)>, Error> {
    let mut reader = Reader::new(bytes);
    let mut fields = Vec::new();
    while let Some((field, value)) = reader
        .next()
        .map_err(|reason| Error::invalid("pprof", reason))?
    {
        let owned = match value {
            Value::Varint(value) => Owned::Varint(value),
            Value::Bytes(value) => Owned::Bytes(value.to_vec()),
        };
        fields.push((field, owned));
    }
    Ok(fields)
}

fn packed_u64(bytes: &[u8]) -> Result<Vec<u64>, Error> {
    decode_varints(bytes).map_err(|reason| Error::invalid("pprof", reason))
}

fn as_i64(value: u64) -> Result<i64, Error> {
    i64::try_from(value)
        .map_err(|_| Error::invalid("pprof", format!("integer {value} does not fit in i64")))
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

pub(crate) fn gunzip_if_needed(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// One pprof sample, still leaf-first, with every value column kept.
pub(crate) struct InspectedSample {
    pub frames: Vec<InspectedFrame>,
    pub values: Vec<i64>,
    /// A thread or goroutine label when the sample actually carries one.
    pub track: Option<InspectedTrack>,
    /// Relative timestamp in nanoseconds when `offset_ns` was recorded on the sample.
    pub offset_ns: Option<u64>,
}

pub(crate) struct InspectedFrame {
    pub runtime: Runtime,
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub address: Option<u64>,
    pub module: Option<String>,
    pub system: bool,
}

pub(crate) struct InspectedTrack {
    pub kind: TrackKind,
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackKind {
    Thread,
    Goroutine,
}

/// Sample types, labels, and mappings that [`import`] drops.
pub(crate) struct InspectedProfile {
    pub sample_types: Vec<(String, String)>,
    pub default_sample_type: usize,
    pub samples: Vec<InspectedSample>,
    pub duration_ns: u64,
    pub period_ns: Option<u64>,
    pub notes: Vec<String>,
}

/// Decode a gzip or raw pprof profile without collapsing value columns.
pub(crate) fn inspect(bytes: &[u8]) -> Result<InspectedProfile, Error> {
    let decoded = gunzip_if_needed(bytes)?;
    let raw = parse_profile(&decoded)?;
    if raw.strings.first().is_some_and(|first| !first.is_empty()) {
        return Err(Error::invalid("pprof", "string_table[0] must be empty"));
    }
    let mut notes = Vec::new();
    let sample_types = raw
        .sample_types
        .iter()
        .map(|(type_index, unit_index)| {
            Ok((
                lookup(&raw.strings, *type_index)?.to_string(),
                lookup(&raw.strings, *unit_index)?.to_string(),
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let default_sample_type = if sample_types.is_empty() {
        0
    } else if raw.default_sample_type < 0 || raw.default_sample_type as usize >= sample_types.len()
    {
        notes.push(format!(
            "default sample type {} is outside the {} declared types; using the first",
            raw.default_sample_type,
            sample_types.len()
        ));
        0
    } else {
        raw.default_sample_type as usize
    };
    let period_ns = if raw.period_nanos > 0 {
        let unit = if raw.period_unit == 0 {
            ""
        } else {
            lookup(&raw.strings, raw.period_unit)?
        };
        match time_scale_ns(unit) {
            Some(scale) => scale_i64(raw.period_nanos, scale).ok(),
            None => {
                if !unit.is_empty() {
                    notes.push(format!(
                        "sampling period uses unit {unit}, which is not a time unit, so it is not shown as a duration"
                    ));
                }
                None
            }
        }
    } else {
        None
    };
    let duration_ns = if raw.duration_nanos > 0 {
        u64::try_from(raw.duration_nanos).unwrap_or(0)
    } else {
        0
    };
    let mut samples = Vec::with_capacity(raw.samples.len());
    for sample in &raw.samples {
        let mut frames = Vec::new();
        for location_id in &sample.locations {
            let location = raw.locations.get(location_id).ok_or_else(|| {
                Error::invalid("pprof", format!("unknown location id {location_id}"))
            })?;
            let module = mapping_name(&raw, location.mapping_id)?;
            let system = module.as_deref().is_some_and(is_system_library);
            if location.lines.is_empty() {
                frames.push(InspectedFrame {
                    runtime: Runtime::Native,
                    name: match none_if_zero(location.address) {
                        Some(addr) => format!("0x{addr:x}"),
                        None => "unknown".to_string(),
                    },
                    file: None,
                    line: None,
                    column: None,
                    address: none_if_zero(location.address),
                    module: module.clone(),
                    system,
                });
                continue;
            }
            for line in &location.lines {
                let mut frame = inspected_frame(&raw, location.address, line)?;
                frame.module = module.clone();
                frame.system = system || frame.runtime == Runtime::Kernel;
                frames.push(frame);
            }
        }
        samples.push(InspectedSample {
            frames,
            values: sample.values.clone(),
            track: track_from_labels(&raw, &sample.labels)?,
            offset_ns: offset_from_labels(&raw, &sample.labels)?,
        });
    }
    Ok(InspectedProfile {
        sample_types,
        default_sample_type,
        samples,
        duration_ns,
        period_ns,
        notes,
    })
}

fn inspected_frame(
    raw: &RawProfile,
    address: u64,
    line: &RawLine,
) -> Result<InspectedFrame, Error> {
    let function = raw.functions.get(&line.function_id).ok_or_else(|| {
        Error::invalid("pprof", format!("unknown function id {}", line.function_id))
    })?;
    let symbol = lookup(&raw.strings, function.name)?;
    let (runtime, name) = parse_symbol(symbol);
    let name = if name.is_empty() {
        match none_if_zero(address) {
            Some(addr) => format!("0x{addr:x}"),
            None => "unknown".to_string(),
        }
    } else {
        name.to_string()
    };
    let file = {
        let filename = lookup(&raw.strings, function.filename)?;
        if filename.is_empty() {
            None
        } else {
            Some(filename.to_string())
        }
    };
    Ok(InspectedFrame {
        runtime,
        name,
        file,
        line: none_if_zero_i64(line.line),
        column: none_if_zero_i64(line.column),
        address: none_if_zero(address),
        module: None,
        system: runtime == Runtime::Kernel,
    })
}

fn mapping_name(raw: &RawProfile, mapping_id: u64) -> Result<Option<String>, Error> {
    let Some(index) = raw.mappings.get(&mapping_id) else {
        return Ok(None);
    };
    let name = lookup(&raw.strings, *index)?;
    if name.is_empty() {
        Ok(None)
    } else {
        Ok(Some(name.to_string()))
    }
}

fn offset_from_labels(raw: &RawProfile, labels: &[RawLabel]) -> Result<Option<u64>, Error> {
    for label in labels {
        let key = lookup(&raw.strings, label.key)?;
        if !key.eq_ignore_ascii_case("offset_ns") {
            continue;
        }
        if label.str_idx != 0 && !label.has_num {
            let text = lookup(&raw.strings, label.str_idx)?.trim();
            if let Ok(parsed) = text.parse::<u64>() {
                return Ok(Some(parsed));
            }
            continue;
        }
        if label.num < 0 {
            return Err(Error::invalid(
                "pprof",
                format!("negative offset_ns {}", label.num),
            ));
        }
        // Note: proto3 omits `num = 0` on the wire, so `label.num == 0` with
        // `key == "offset_ns"` and `str_idx == 0` encodes `0ns`.
        let scale = if label.num_unit != 0 {
            let unit = lookup(&raw.strings, label.num_unit)?;
            time_scale_ns(unit).unwrap_or(1.0)
        } else {
            1.0
        };
        return Ok(Some(scale_i64(label.num, scale)?));
    }
    Ok(None)
}

fn thread_id_and_name(
    raw: &RawProfile,
    labels: &[RawLabel],
) -> Result<(u64, Option<String>), Error> {
    for label in labels {
        let key = lookup(&raw.strings, label.key)?;
        if track_kind(key) != Some(TrackKind::Thread) {
            continue;
        }
        let text = if label.str_idx != 0 {
            lookup(&raw.strings, label.str_idx)?.trim().to_string()
        } else {
            String::new()
        };
        let mut tid = if label.has_num && label.num > 0 {
            u64::try_from(label.num).unwrap_or(0)
        } else {
            0
        };
        if tid == 0 {
            tid = parse_tid_from_thread_label(&text).unwrap_or(0);
        }
        let name = if text.is_empty() || (tid > 0 && text == format!("Thread {tid}")) {
            None
        } else if tid > 0 {
            let suffix = format!(" ({tid})");
            let trimmed = text.strip_suffix(&suffix).unwrap_or(&text).trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        } else {
            Some(text)
        };
        return Ok((tid, name));
    }
    Ok((0, None))
}

pub(crate) fn parse_tid_from_thread_label(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("Thread ") {
        return rest.trim().parse::<u64>().ok();
    }
    if let Some((_prefix, tail)) = trimmed.rsplit_once(" (")
        && let Some(digits) = tail.strip_suffix(')')
    {
        return digits.parse::<u64>().ok();
    }
    None
}

fn track_from_labels(
    raw: &RawProfile,
    labels: &[RawLabel],
) -> Result<Option<InspectedTrack>, Error> {
    for label in labels {
        let key = lookup(&raw.strings, label.key)?;
        let Some(kind) = track_kind(key) else {
            continue;
        };
        let text = if label.str_idx != 0 {
            lookup(&raw.strings, label.str_idx)?.to_string()
        } else {
            String::new()
        };
        let name = if !text.is_empty() {
            text
        } else if label.has_num {
            label.num.to_string()
        } else {
            continue;
        };
        return Ok(Some(InspectedTrack { kind, name }));
    }
    Ok(None)
}

fn track_kind(key: &str) -> Option<TrackKind> {
    match key.to_ascii_lowercase().as_str() {
        "thread" | "thread id" | "threadid" | "tid" | "os thread" | "osthread" => {
            Some(TrackKind::Thread)
        }
        "goroutine" => Some(TrackKind::Goroutine),
        _ => None,
    }
}

/// Nanoseconds in one of `unit`, when `unit` is a time unit.
pub(crate) fn time_scale_ns(unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "nanoseconds" | "nanosecond" | "ns" => Some(1.0),
        "microseconds" | "microsecond" | "us" | "µs" | "μs" => Some(1_000.0),
        "milliseconds" | "millisecond" | "ms" => Some(1_000_000.0),
        "seconds" | "second" | "s" => Some(1_000_000_000.0),
        _ => None,
    }
}

fn scale_i64(value: i64, scale: f64) -> Result<u64, Error> {
    if value < 0 {
        return Err(Error::invalid("pprof", format!("negative value {value}")));
    }
    ticks_from_f64(value as f64 * scale, "pprof")
}

pub(crate) fn ticks_from_f64(value: f64, format: &'static str) -> Result<u64, Error> {
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
        return Err(Error::invalid(
            format,
            format!("value {value} is not a duration"),
        ));
    }
    Ok(value.round() as u64)
}

/// Operating-system libraries anyprof can recognize from a mapped path or module basename.
///
/// A bare function name is not enough: `std::` and `java.` are not marked.
pub(crate) fn is_system_library(path: &str) -> bool {
    let lower = path.trim().replace('\\', "/").to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    const WINDOWS: &[&str] = &[
        "ntdll.dll",
        "kernel32.dll",
        "kernelbase.dll",
        "win32u.dll",
        "ucrtbase.dll",
        "msvcrt.dll",
        "advapi32.dll",
        "user32.dll",
        "ws2_32.dll",
        "rpcrt4.dll",
        "sechost.dll",
        "bcryptprimitives.dll",
    ];
    if WINDOWS.contains(&base) {
        return true;
    }
    if is_unix_system_basename(base) {
        return true;
    }
    const PREFIXES: &[&str] = &[
        "/usr/lib/",
        "/usr/lib64/",
        "/lib/",
        "/lib64/",
        "/system/library/",
        "/usr/lib/system/",
        "/library/apple/",
        "c:/windows/system32/",
        "c:/windows/syswow64/",
        "/windows/system32/",
        "/windows/syswow64/",
    ];
    PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
        || lower.contains("/windows/system32/")
        || lower.contains("/windows/syswow64/")
}

fn is_unix_system_basename(base: &str) -> bool {
    if matches!(
        base,
        "[vdso]"
            | "linux-vdso.so.1"
            | "libc.so"
            | "libm.so"
            | "libpthread.so"
            | "libdl.so"
            | "librt.so"
            | "libdyld.dylib"
            | "libobjc.a.dylib"
            | "libdispatch.dylib"
            | "libxpc.dylib"
            | "libmacho.dylib"
            | "libquarantine.dylib"
            | "libcommoncrypto.dylib"
    ) {
        return true;
    }
    if base.starts_with("libc.so.")
        || base.starts_with("libm.so.")
        || base.starts_with("libpthread.so.")
        || base.starts_with("libdl.so.")
        || base.starts_with("librt.so.")
        || base.starts_with("ld-linux")
        || base.starts_with("ld-musl")
        || (base.starts_with("libc-") && base.ends_with(".so"))
        || (base.starts_with("libm-") && base.ends_with(".so"))
        || (base.starts_with("libpthread-") && base.ends_with(".so"))
        || (base.starts_with("libsystem_") && base.ends_with(".dylib"))
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{export, is_system_library};
    use crate::{Format, import};
    use anyprof_core::{Frame, Profile, Runtime, Stack, TimedSample};
    use std::time::Duration;

    #[test]
    fn roundtrip_stacks_duration_and_period() {
        let original = crate::fixture();
        let bytes = export(&original).unwrap();
        assert!(
            bytes.starts_with(&[0x1f, 0x8b]),
            "pprof output is gzip-compressed"
        );

        let profile = import(&bytes, Format::Pprof).unwrap();
        assert_eq!(profile.duration, Duration::from_secs(5));
        assert_eq!(profile.period, Some(Duration::from_millis(10)));
        assert_eq!(profile.stacks.len(), 3);
        assert_eq!(profile.total_weight(), 7);

        let leaf = &profile.stacks[0].frames[0];
        assert_eq!(leaf.runtime, Runtime::Node);
        assert_eq!(leaf.name, "work");
        assert_eq!(leaf.file.as_deref(), Some("app.js"));
        assert_eq!(leaf.line, Some(12));
        assert_eq!(leaf.column, Some(3));
        let root = &profile.stacks[0].frames[1];
        assert_eq!(root.runtime, Runtime::Native);
        assert_eq!(root.name, "main");
        assert_eq!(root.address, Some(0x1000));
        assert_eq!(profile.stacks[2].frames[0].runtime, Runtime::Go);
        assert_eq!(profile.stacks[2].frames[1].runtime, Runtime::Kernel);
    }

    #[test]
    fn accepts_an_uncompressed_payload() {
        let bytes = export(&crate::fixture()).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut raw).unwrap();
        let profile = import(&raw, Format::Pprof).unwrap();
        assert_eq!(profile.total_weight(), 7);
    }

    #[test]
    fn roundtrip_preserves_mappings_thread_labels_and_timed_samples() {
        let mut libc = Frame::new(Runtime::Native, "pthread_mutex_lock");
        libc.module = Some("libc.so.6".to_string());
        let mut worker = Frame::new(Runtime::Rust, "demo::worker");
        worker.module = Some("demo".to_string());
        worker.file = Some("src/main.rs".to_string());
        worker.line = Some(42);

        let profile = Profile {
            pid: 900,
            comm: Some("demo".to_string()),
            duration: Duration::from_millis(100),
            period: Some(Duration::from_millis(10)),
            stacks: vec![
                Stack {
                    frames: vec![worker.clone()],
                    weight: 2,
                },
                Stack {
                    frames: vec![libc.clone(), worker.clone()],
                    weight: 1,
                },
            ],
            samples: vec![
                TimedSample {
                    tid: 1001,
                    thread_name: Some("worker-1".to_string()),
                    offset_ns: 0,
                    stack_index: 0,
                    weight: 1,
                },
                TimedSample {
                    tid: 1002,
                    thread_name: None,
                    offset_ns: 20_000_000,
                    stack_index: 1,
                    weight: 1,
                },
                TimedSample {
                    tid: 1001,
                    thread_name: Some("worker-1".to_string()),
                    offset_ns: 50_000_000,
                    stack_index: 0,
                    weight: 1,
                },
            ],
            raw_cpuprofile: None,
        };
        let bytes = export(&profile).unwrap();
        let decoded = import(&bytes, Format::Pprof).unwrap();
        assert_eq!(decoded.total_weight(), 3);
        assert_eq!(decoded.samples.len(), 3);
        assert_eq!(decoded.samples[0].offset_ns, 0);
        assert_eq!(decoded.samples[0].tid, 1001);
        assert_eq!(decoded.samples[0].thread_name.as_deref(), Some("worker-1"));
        assert_eq!(decoded.samples[1].offset_ns, 20_000_000);
        assert_eq!(decoded.samples[1].tid, 1002);
        assert_eq!(decoded.samples[1].thread_name, None);
        assert_eq!(decoded.samples[2].offset_ns, 50_000_000);

        let libc_stack = decoded
            .stacks
            .iter()
            .find(|s| s.frames[0].name == "pthread_mutex_lock")
            .unwrap();
        assert_eq!(libc_stack.frames[0].module.as_deref(), Some("libc.so.6"));
        assert_eq!(libc_stack.frames[1].module.as_deref(), Some("demo"));

        assert!(is_system_library("libc.so.6"));
        assert!(is_system_library("libsystem_kernel.dylib"));
        assert!(is_system_library("ntdll.dll"));
        assert!(!is_system_library("demo"));
        assert!(!is_system_library("libpython3.12.so.1.0"));
    }
}
