//! gzip-compressed pprof profiles (`perftools.profiles.Profile`).
//!
//! The written subset is sample type `samples/count`, one location per frame,
//! function name/file/line, optional address, duration, and period. Period is
//! stored in nanoseconds. Mappings and the per-frame module are left empty.
//!
//! TODO(export): emit pprof mappings so `module` survives a roundtrip.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use anyprof_core::{Frame, Profile, Runtime, Stack};
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

    let mut functions: HashMap<(u64, u64, i64), u64> = HashMap::new();
    let mut function_msgs = Vec::new();
    let mut locations: HashMap<(u64, i64, i64, u64), u64> = HashMap::new();
    let mut location_msgs = Vec::new();
    let mut sample_msgs = Vec::new();

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

    let mut message = Writer::new();
    message.field_message(1, &value_type(samples_label, count_label));
    for sample in &sample_msgs {
        message.field_message(2, sample);
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

pub(crate) fn import(bytes: &[u8]) -> Result<Profile, Error> {
    let decoded = gunzip_if_needed(bytes)?;
    let mut raw = parse_profile(&decoded)?;
    if raw.strings.first().is_some_and(|first| !first.is_empty()) {
        return Err(Error::invalid("pprof", "string_table[0] must be empty"));
    }

    let samples = std::mem::take(&mut raw.samples);
    let mut stacks = Vec::with_capacity(samples.len());
    for sample in samples {
        let weight = match sample.values.first() {
            Some(value) if *value >= 0 => u64::try_from(*value).unwrap_or(0),
            Some(value) => {
                return Err(Error::invalid(
                    "pprof",
                    format!("negative sample value {value}"),
                ));
            }
            None => 0,
        };
        let mut frames = Vec::new();
        for location_id in sample.locations {
            let location = raw.locations.get(&location_id).ok_or_else(|| {
                Error::invalid("pprof", format!("unknown location id {location_id}"))
            })?;
            if location.lines.is_empty() {
                frames.push(Frame {
                    address: none_if_zero(location.address),
                    ..Frame::new(Runtime::Native, "")
                });
                continue;
            }
            for line in &location.lines {
                frames.push(frame_from_line(&raw, location.address, line)?);
            }
        }
        stacks.push(Stack { frames, weight });
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
        ..Profile::default()
    })
}

fn intern_location(
    frame: &Frame,
    strings: &mut StringTable,
    functions: &mut HashMap<(u64, u64, i64), u64>,
    function_msgs: &mut Vec<Writer>,
    locations: &mut HashMap<(u64, i64, i64, u64), u64>,
    location_msgs: &mut Vec<Writer>,
) -> u64 {
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

    let location_key = (function_id, start_line, column, address);
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
    samples: Vec<RawSample>,
    locations: HashMap<u64, RawLocation>,
    functions: HashMap<u64, RawFunction>,
    strings: Vec<String>,
    duration_nanos: i64,
    period_nanos: i64,
    period_unit: i64,
}

struct RawSample {
    locations: Vec<u64>,
    values: Vec<i64>,
}

struct RawLocation {
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
        samples: Vec::new(),
        locations: HashMap::new(),
        functions: HashMap::new(),
        strings: Vec::new(),
        duration_nanos: 0,
        period_nanos: 0,
        period_unit: 0,
    };
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (2, Owned::Bytes(sample)) => raw.samples.push(parse_sample(&sample)?),
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
                raw.period_unit = parse_value_type(&period_type)?.1;
            }
            (12, Owned::Varint(period)) => raw.period_nanos = as_i64(period)?,
            _ => {}
        }
    }
    Ok(raw)
}

fn parse_sample(bytes: &[u8]) -> Result<RawSample, Error> {
    let mut sample = RawSample {
        locations: Vec::new(),
        values: Vec::new(),
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
            _ => {}
        }
    }
    Ok(sample)
}

fn parse_location(bytes: &[u8]) -> Result<(u64, RawLocation), Error> {
    let mut id = 0_u64;
    let mut address = 0_u64;
    let mut lines = Vec::new();
    for (field, value) in message_fields(bytes)? {
        match (field, value) {
            (1, Owned::Varint(value)) => id = value,
            (3, Owned::Varint(value)) => address = value,
            (4, Owned::Bytes(line)) => lines.push(parse_line(&line)?),
            _ => {}
        }
    }
    if id == 0 {
        return Err(Error::invalid("pprof", "location id 0"));
    }
    Ok((id, RawLocation { address, lines }))
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

fn gunzip_if_needed(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::export;
    use crate::{Format, import};
    use anyprof_core::Runtime;
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
}
