//! Timed view of a profile, for the local viewer.
//!
//! [`crate::import`] folds samples into weighted stacks and drops timestamps,
//! thread labels, and pprof value columns. This module keeps a time axis only
//! when the file itself is a series: repeated V8 samples with `timeDeltas`,
//! speedscope events, or a speedscope sample list that repeats stacks on a
//! time unit. Folded stacks stay an aggregate. Sample order, `timeDelta`
//! values copied from counts, and `period * count` are not a timeline or a
//! CPU curve. Thread tracks come from labels or separate profiles in the
//! file, not from the order of folded stacks.

use std::collections::{HashMap, HashSet};

use anyprof_core::{Frame, Runtime};
use serde_json::Value;

use crate::error::Error;
use crate::pprof::{self, TrackKind as PprofTrack};
use crate::symbol::split_runtime_prefix;

const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_SAMPLES: usize = 5_000_000;
const MAX_DEPTH: usize = 8_192;

/// What a weight means. The viewer uses this instead of guessing CPU usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeightKind {
    /// Sample or event durations laid out on a time axis.
    SampledTime,
    /// Aggregated pprof `cpu` time. There is still no per-sample timeline.
    CpuTime,
    /// Aggregated time that is not labeled `cpu` and has no timestamps.
    Duration,
    /// Sample counts, hit counts, or a speedscope unit of `none`.
    Count,
    /// Byte weights.
    Bytes,
    /// A unit this viewer does not treat as time, count, or bytes.
    Other,
}

impl WeightKind {
    /// Stable name used in the viewer JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SampledTime => "sampled-time",
            Self::CpuTime => "cpu-time",
            Self::Duration => "duration",
            Self::Count => "count",
            Self::Bytes => "bytes",
            Self::Other => "other",
        }
    }
}

/// How a track was identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// A pprof thread label.
    Thread,
    /// A pprof goroutine label.
    Goroutine,
    /// One profile, or a file that never named a thread.
    Profile,
}

impl TrackKind {
    /// Stable name used in the viewer JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::Goroutine => "goroutine",
            Self::Profile => "profile",
        }
    }
}

/// One function, after symbols have been split into a runtime and a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFrame {
    /// Function or symbol. Never empty.
    pub name: String,
    /// Source file, when the profile has one.
    pub file: Option<String>,
    /// 1-based line, when the profile has one.
    pub line: Option<u32>,
    /// 1-based column, when the profile has one.
    pub column: Option<u32>,
    /// Runtime from a `[runtime]` prefix, or the format's default.
    pub runtime: Runtime,
    /// Mapped library, when pprof recorded one.
    pub module: Option<String>,
    /// Instruction address, when pprof recorded one.
    pub address: Option<u64>,
    /// `true` only for a kernel frame or a system-library mapping.
    pub system: bool,
}

/// One thread, goroutine, or unnamed profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewThread {
    /// Label from the file, or `Samples` when the file has a single unnamed track.
    pub name: String,
    /// Why this track exists.
    pub kind: TrackKind,
}

/// A profile the viewer can filter by time and thread.
#[derive(Debug, Clone)]
pub struct Trace {
    /// `cpuprofile`, `speedscope`, `pprof`, or `collapsed`.
    pub format: &'static str,
    /// How to label weights.
    pub weight_kind: WeightKind,
    /// Unit token from the file, such as `microseconds`, `none`, or `count`.
    pub unit: String,
    /// Inclusive start and exclusive end, in ticks, when samples have times.
    pub timeline: Option<(u64, u64)>,
    /// Why [`Self::timeline`] is `None`.
    pub timeline_disabled: Option<String>,
    /// Interned frames.
    pub frames: Vec<ViewFrame>,
    /// Tracks in first-seen order.
    pub threads: Vec<ViewThread>,
    stack_frames: Vec<u32>,
    stacks: Vec<(u32, u32)>,
    samples: Vec<Sample>,
    /// Sum of pprof `cpu` time, in nanoseconds, when that column exists.
    pub cpu_ns: Option<u64>,
    /// Profile wall duration, in nanoseconds, when the file records one.
    pub wall_ns: Option<u64>,
    /// Sampling period, in nanoseconds, when its unit is a time unit.
    pub period_ns: Option<u64>,
    /// Warnings that do not make the file unreadable.
    pub notes: Vec<String>,
    frame_index: HashMap<FrameKey, u32>,
    stack_index: HashMap<Vec<u32>, u32>,
}

#[derive(Debug, Clone)]
struct Sample {
    thread: u32,
    stack: u32,
    start: u64,
    weight: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FrameKey {
    name: String,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    runtime: Runtime,
    module: Option<String>,
    address: Option<u64>,
    system: bool,
}

/// A filter over a [`Trace`].
///
/// Time bounds are ticks (nanoseconds for time units, or raw counts otherwise)
/// and the end is exclusive. `None` means that side is unbounded. Time bounds
/// are ignored when the trace has no timeline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    /// Inclusive start tick.
    pub start: Option<u64>,
    /// Exclusive end tick.
    pub end: Option<u64>,
    /// Tracks to keep. `None` keeps every track.
    pub threads: Option<Vec<u32>>,
    /// Put the leaf at the root of the call tree.
    pub invert: bool,
    /// Drop kernel frames and frames mapped from a system library.
    pub hide_system: bool,
}

/// Call tree, flat functions, and the heaviest stack for one [`Query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    /// Sum of the weights that fell inside the query.
    pub total: u64,
    /// Sample records that contributed a non-zero weight.
    pub samples: u64,
    /// Preorder call tree. Parents come before their children.
    pub nodes: Vec<TreeNode>,
    /// One row per function, counting a recursive frame once per sample.
    pub functions: Vec<FunctionRow>,
    /// Heaviest stack in display order (root first, or leaf first when inverted).
    pub heaviest: Vec<u32>,
}

/// One call-tree row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Index into [`Aggregate::nodes`].
    pub id: u32,
    /// Parent row, or `None` for a root.
    pub parent: Option<u32>,
    /// Index into [`Trace::frames`].
    pub frame: u32,
    /// Zero for a root.
    pub depth: u32,
    /// Inclusive weight.
    pub weight: u64,
    /// Weight of samples whose leaf is this row.
    pub self_weight: u64,
}

/// Inclusive and self weight of one function, merged across stacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionRow {
    /// Index into [`Trace::frames`].
    pub frame: u32,
    /// Inclusive weight. A recursive function counts once per sample.
    pub weight: u64,
    /// Weight of samples where this function is the leaf.
    pub self_weight: u64,
}

/// Sample weight in each bucket of a time window, one row per track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Inclusive start tick.
    pub start: u64,
    /// Exclusive end tick.
    pub end: u64,
    /// `rows[thread][bucket]`.
    pub rows: Vec<Vec<u64>>,
}

impl Trace {
    fn new(format: &'static str, weight_kind: WeightKind, unit: impl Into<String>) -> Self {
        Self {
            format,
            weight_kind,
            unit: unit.into(),
            timeline: None,
            timeline_disabled: None,
            frames: Vec::new(),
            threads: Vec::new(),
            stack_frames: Vec::new(),
            stacks: Vec::new(),
            samples: Vec::new(),
            cpu_ns: None,
            wall_ns: None,
            period_ns: None,
            notes: Vec::new(),
            frame_index: HashMap::new(),
            stack_index: HashMap::new(),
        }
    }

    /// Number of sample records, not the sum of their weights.
    #[must_use]
    pub fn sample_records(&self) -> u64 {
        self.samples.len() as u64
    }

    /// Sum of sample weights, ignoring any later filter.
    #[must_use]
    pub fn total_weight(&self) -> u64 {
        self.samples
            .iter()
            .fold(0, |sum, sample| sum.saturating_add(sample.weight))
    }

    /// Whether any frame is classified as a system frame.
    #[must_use]
    pub fn has_system_frames(&self) -> bool {
        self.frames.iter().any(|frame| frame.system)
    }

    fn add_thread(&mut self, name: impl Into<String>, kind: TrackKind) -> u32 {
        let id = self.threads.len() as u32;
        self.threads.push(ViewThread {
            name: name.into(),
            kind,
        });
        id
    }

    fn intern(&mut self, frame: ViewFrame) -> u32 {
        let key = FrameKey {
            name: frame.name.clone(),
            file: frame.file.clone(),
            line: frame.line,
            column: frame.column,
            runtime: frame.runtime,
            module: frame.module.clone(),
            address: if frame.name.is_empty() {
                frame.address
            } else {
                None
            },
            system: frame.system,
        };
        if let Some(id) = self.frame_index.get(&key) {
            return *id;
        }
        let id = self.frames.len() as u32;
        self.frames.push(frame);
        self.frame_index.insert(key, id);
        id
    }

    fn intern_normalized(&mut self, mut frame: ViewFrame) -> u32 {
        if frame.name.is_empty() {
            frame.name = match frame.address {
                Some(addr) => format!("0x{addr:x}"),
                None => "unknown".to_string(),
            };
        }
        frame.system = frame.system || frame.runtime == Runtime::Kernel;
        self.intern(frame)
    }

    fn intern_core(&mut self, frame: &Frame) -> u32 {
        self.intern_normalized(ViewFrame {
            name: frame.name.clone(),
            file: frame.file.clone(),
            line: frame.line,
            column: frame.column,
            runtime: frame.runtime,
            module: frame.module.clone(),
            address: frame.address,
            system: frame.runtime == Runtime::Kernel,
        })
    }

    fn intern_stack(&mut self, frames: &[u32]) -> u32 {
        if let Some(id) = self.stack_index.get(frames) {
            return *id;
        }
        let id = self.stacks.len() as u32;
        let start = self.stack_frames.len() as u32;
        self.stack_frames.extend_from_slice(frames);
        self.stacks.push((start, frames.len() as u32));
        self.stack_index.insert(frames.to_vec(), id);
        id
    }

    fn add_sample(
        &mut self,
        thread: u32,
        frames: &[u32],
        start: u64,
        weight: u64,
    ) -> Result<(), Error> {
        if weight == 0 || frames.is_empty() {
            return Ok(());
        }
        if self.samples.len() >= MAX_SAMPLES {
            return Err(Error::invalid(
                "profile",
                format!("profile has more than {MAX_SAMPLES} samples"),
            ));
        }
        let stack = self.intern_stack(frames);
        self.samples.push(Sample {
            thread,
            stack,
            start,
            weight,
        });
        Ok(())
    }

    fn stack(&self, id: u32) -> &[u32] {
        let (start, len) = self.stacks[id as usize];
        let start = start as usize;
        &self.stack_frames[start..start + len as usize]
    }
}

/// Read a cpuprofile, speedscope, pprof, or collapsed-stack profile.
///
/// Gzip is accepted for every format. The format is chosen from the contents.
pub fn load_trace(bytes: &[u8]) -> Result<Trace, Error> {
    if bytes.is_empty() {
        return Err(Error::invalid("profile", "file is empty"));
    }
    if bytes.len() > MAX_BYTES {
        return Err(Error::invalid("profile", "profile is larger than 256 MiB"));
    }
    let raw = unpack(bytes)?;
    if raw.len() > MAX_BYTES {
        return Err(Error::invalid(
            "profile",
            "decompressed profile is larger than 256 MiB",
        ));
    }
    let body = trim_bom(&raw);
    if looks_like_json(body) {
        return load_json(body);
    }
    if let Some(trace) = try_collapsed(&raw)? {
        return Ok(trace);
    }
    match from_pprof(&raw) {
        Ok(trace) => Ok(trace),
        Err(pprof) => Err(Error::invalid(
            "profile",
            format!("could not read this file as a CPU profile ({pprof})"),
        )),
    }
}

/// Build the call tree for `query`.
#[must_use]
pub fn aggregate(trace: &Trace, query: &Query) -> Aggregate {
    let allow = query.threads.as_ref().map(|ids| {
        ids.iter()
            .copied()
            .filter(|id| (*id as usize) < trace.threads.len())
            .collect::<HashSet<_>>()
    });
    let timed = trace.timeline.is_some();
    let mut nodes: Vec<BuildNode> = Vec::new();
    let mut child: HashMap<(u32, u32), u32> = HashMap::new();
    let mut functions: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut heaviest_weight: HashMap<u32, u64> = HashMap::new();
    let mut total = 0_u64;
    let mut counted = 0_u64;
    let mut filtered = Vec::new();
    let mut seen = HashSet::new();

    for sample in &trace.samples {
        if allow
            .as_ref()
            .is_some_and(|ids| !ids.contains(&sample.thread))
        {
            continue;
        }
        let weight = if timed {
            overlap(sample.start, sample.weight, query.start, query.end)
        } else {
            sample.weight
        };
        if weight == 0 {
            continue;
        }
        filtered.clear();
        for frame in trace.stack(sample.stack) {
            if query.hide_system && trace.frames[*frame as usize].system {
                continue;
            }
            filtered.push(*frame);
        }
        if filtered.is_empty() {
            continue;
        }
        counted = counted.saturating_add(1);
        total = total.saturating_add(weight);
        let stack_id = sample.stack;
        let summed = heaviest_weight.entry(stack_id).or_insert(0);
        *summed = summed.saturating_add(weight);

        seen.clear();
        for frame in &filtered {
            if seen.insert(*frame) {
                let slot = functions.entry(*frame).or_insert((0, 0));
                slot.0 = slot.0.saturating_add(weight);
            }
        }
        let leaf = *filtered.last().expect("filtered stack is non-empty");
        let slot = functions.get_mut(&leaf).expect("leaf was inserted");
        slot.1 = slot.1.saturating_add(weight);

        let self_at = if query.invert { 0 } else { filtered.len() - 1 };
        if query.invert {
            filtered.reverse();
        }
        let mut parent = u32::MAX;
        for (index, frame) in filtered.iter().copied().enumerate() {
            let id = if let Some(id) = child.get(&(parent, frame)) {
                *id
            } else {
                let id = nodes.len() as u32;
                nodes.push(BuildNode {
                    frame,
                    parent: if parent == u32::MAX {
                        None
                    } else {
                        Some(parent)
                    },
                    weight: 0,
                    self_weight: 0,
                    depth: index as u32,
                    children: Vec::new(),
                });
                if parent != u32::MAX {
                    nodes[parent as usize].children.push(id);
                }
                child.insert((parent, frame), id);
                id
            };
            let node = &mut nodes[id as usize];
            node.weight = node.weight.saturating_add(weight);
            if index == self_at {
                node.self_weight = node.self_weight.saturating_add(weight);
            }
            parent = id;
        }
    }

    let mut best_weight = 0_u64;
    let mut best_stack = None;
    for (stack, weight) in heaviest_weight {
        let better = best_stack
            .is_none_or(|best| weight > best_weight || (weight == best_weight && stack < best));
        if better {
            best_weight = weight;
            best_stack = Some(stack);
        }
    }
    let heaviest = best_stack
        .map(|stack| {
            let mut frames: Vec<u32> = trace
                .stack(stack)
                .iter()
                .copied()
                .filter(|frame| !query.hide_system || !trace.frames[*frame as usize].system)
                .collect();
            if query.invert {
                frames.reverse();
            }
            frames
        })
        .unwrap_or_default();

    let mut order = Vec::with_capacity(nodes.len());
    let mut roots: Vec<u32> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.parent.is_none())
        .map(|(id, _)| id as u32)
        .collect();
    sort_ids(&mut roots, &nodes);
    for root in roots {
        sort_children(root, &mut nodes);
        push_preorder(root, &nodes, &mut order);
    }
    let mut remap = HashMap::with_capacity(order.len());
    for (new_id, old_id) in order.iter().enumerate() {
        remap.insert(*old_id, new_id as u32);
    }
    let tree = order
        .into_iter()
        .map(|old| {
            let node = &nodes[old as usize];
            TreeNode {
                id: remap[&old],
                parent: node.parent.map(|parent| remap[&parent]),
                frame: node.frame,
                depth: node.depth,
                weight: node.weight,
                self_weight: node.self_weight,
            }
        })
        .collect();
    let mut functions: Vec<FunctionRow> = functions
        .into_iter()
        .map(|(frame, (weight, self_weight))| FunctionRow {
            frame,
            weight,
            self_weight,
        })
        .collect();
    functions.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then(left.frame.cmp(&right.frame))
    });
    Aggregate {
        total,
        samples: counted,
        nodes: tree,
        functions,
        heaviest,
    }
}

/// Bucket the sample activity in `[start, end)`.
///
/// Fails when the profile has no time axis. Buckets split a sample by how much
/// of its interval falls in each bucket; they are not CPU percent.
pub fn activity(trace: &Trace, start: u64, end: u64, buckets: usize) -> Result<Activity, Error> {
    if trace.timeline.is_none() {
        return Err(Error::invalid(trace.format, "this profile has no timeline"));
    }
    let buckets = buckets.clamp(1, 1_000);
    if end <= start {
        return Ok(Activity {
            start,
            end,
            rows: vec![vec![0; buckets]; trace.threads.len()],
        });
    }
    let mut rows = vec![vec![0_u64; buckets]; trace.threads.len()];
    let span = end - start;
    for sample in &trace.samples {
        let thread = sample.thread as usize;
        if thread >= rows.len() {
            continue;
        }
        let left = sample.start.max(start);
        let right = sample.start.saturating_add(sample.weight).min(end);
        if right <= left {
            continue;
        }
        let first = bucket_index(left, start, span, buckets);
        let last = bucket_index(right - 1, start, span, buckets);
        for (bucket, cell) in rows[thread]
            .iter_mut()
            .enumerate()
            .take(last + 1)
            .skip(first)
        {
            let (lo, hi) = bucket_bounds(start, span, buckets, bucket);
            let from = left.max(lo);
            let to = right.min(hi);
            if to > from {
                *cell = cell.saturating_add(to - from);
            }
        }
    }
    Ok(Activity { start, end, rows })
}

struct BuildNode {
    frame: u32,
    parent: Option<u32>,
    weight: u64,
    self_weight: u64,
    depth: u32,
    children: Vec<u32>,
}

fn overlap(start: u64, weight: u64, query_start: Option<u64>, query_end: Option<u64>) -> u64 {
    let end = start.saturating_add(weight);
    let left = query_start.map_or(start, |bound| start.max(bound));
    let right = query_end.map_or(end, |bound| end.min(bound));
    right.saturating_sub(left)
}

fn bucket_index(tick: u64, start: u64, span: u64, buckets: usize) -> usize {
    let offset = tick.saturating_sub(start);
    let index = (u128::from(offset) * buckets as u128) / u128::from(span);
    (index as usize).min(buckets - 1)
}

fn bucket_bounds(start: u64, span: u64, buckets: usize, bucket: usize) -> (u64, u64) {
    let buckets = buckets as u128;
    let bucket = bucket as u128;
    let span = u128::from(span);
    let start = u128::from(start);
    let lo = start + span * bucket / buckets;
    let hi = start + span * (bucket + 1) / buckets;
    (lo as u64, hi as u64)
}

fn sort_ids(ids: &mut [u32], nodes: &[BuildNode]) {
    ids.sort_by(|left, right| {
        let a = &nodes[*left as usize];
        let b = &nodes[*right as usize];
        b.weight
            .cmp(&a.weight)
            .then(a.frame.cmp(&b.frame))
            .then(left.cmp(right))
    });
}

fn sort_children(id: u32, nodes: &mut [BuildNode]) {
    let mut children = std::mem::take(&mut nodes[id as usize].children);
    sort_ids(&mut children, nodes);
    nodes[id as usize].children = children;
    let children = nodes[id as usize].children.clone();
    for child in children {
        sort_children(child, nodes);
    }
}

fn push_preorder(id: u32, nodes: &[BuildNode], out: &mut Vec<u32>) {
    out.push(id);
    for child in &nodes[id as usize].children {
        push_preorder(*child, nodes, out);
    }
}

fn unpack(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let read = std::io::Read::read(&mut decoder, &mut buf)
                .map_err(|err| Error::invalid("profile", format!("gzip: {err}")))?;
            if read == 0 {
                break;
            }
            if out.len().saturating_add(read) > MAX_BYTES {
                return Err(Error::invalid(
                    "profile",
                    "decompressed profile is larger than 256 MiB",
                ));
            }
            out.extend_from_slice(&buf[..read]);
        }
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

fn trim_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)
}

fn looks_like_json(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{')
}

fn load_json(bytes: &[u8]) -> Result<Trace, Error> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|err| Error::invalid("profile", format!("invalid JSON: {}", brief(&err))))?;
    if value.pointer("/shared/frames").is_some() && value.get("profiles").is_some() {
        return from_speedscope(&value);
    }
    if value.get("nodes").is_some() || value.pointer("/profile/nodes").is_some() {
        return from_cpuprofile(&value);
    }
    Err(Error::invalid(
        "profile",
        "JSON is neither a V8 CPU profile nor a speedscope file",
    ))
}

fn try_collapsed(bytes: &[u8]) -> Result<Option<Trace>, Error> {
    if bytes.contains(&0) {
        return Ok(None);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    let data: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    if data.is_empty() {
        return Ok(Some(empty_collapsed()));
    }
    let matched = data.iter().filter(|line| is_collapsed_line(line)).count();
    if matched == 0 {
        return Ok(None);
    }
    if matched != data.len() {
        return Err(crate::collapsed::import(bytes).unwrap_err());
    }
    Ok(Some(from_collapsed_profile(crate::collapsed::import(
        bytes,
    )?)?))
}

fn is_collapsed_line(line: &str) -> bool {
    let Some((stack, count)) = line.rsplit_once(' ') else {
        return false;
    };
    !stack.is_empty() && count.parse::<u64>().is_ok()
}

fn empty_collapsed() -> Trace {
    let mut trace = Trace::new("collapsed", WeightKind::Count, "count");
    trace.timeline_disabled =
        Some("Collapsed stacks only record how often each stack was seen, not when.".to_string());
    trace
}

fn from_collapsed_profile(profile: anyprof_core::Profile) -> Result<Trace, Error> {
    let mut trace = empty_collapsed();
    if profile.stacks.is_empty() {
        return Ok(trace);
    }
    let thread = trace.add_thread("Samples", TrackKind::Profile);
    for stack in &profile.stacks {
        let mut frames = Vec::with_capacity(stack.frames.len());
        for frame in stack.frames.iter().rev() {
            frames.push(trace.intern_core(frame));
        }
        trace.add_sample(thread, &frames, 0, stack.weight)?;
    }
    trace
        .notes
        .push("This file does not name threads, source lines, or sample times.".to_string());
    Ok(trace)
}

fn from_pprof(bytes: &[u8]) -> Result<Trace, Error> {
    let inspected = pprof::inspect(bytes)?;
    let type_pair = inspected
        .sample_types
        .get(inspected.default_sample_type)
        .cloned()
        .unwrap_or_else(|| ("samples".to_string(), "count".to_string()));
    let scale = pprof::time_scale_ns(&type_pair.1);
    let weight_kind = if type_pair.0.eq_ignore_ascii_case("cpu") && scale.is_some() {
        WeightKind::CpuTime
    } else if scale.is_some() {
        WeightKind::Duration
    } else if type_pair.1.eq_ignore_ascii_case("bytes") || type_pair.0.eq_ignore_ascii_case("bytes")
    {
        WeightKind::Bytes
    } else if type_pair.1.eq_ignore_ascii_case("count")
        || type_pair.0.eq_ignore_ascii_case("samples")
        || type_pair.1.is_empty()
    {
        WeightKind::Count
    } else {
        WeightKind::Other
    };
    let mut trace = Trace::new("pprof", weight_kind, type_pair.1.clone());
    trace.notes = inspected.notes;
    trace.timeline_disabled = Some(
        "This pprof profile has one weight per distinct stack and no per-sample timestamps. Thread ids are shown only when a sample is labeled with one. There is no timeline.".to_string(),
    );
    if inspected.duration_ns > 0 {
        trace.wall_ns = Some(inspected.duration_ns);
    }
    trace.period_ns = inspected.period_ns;
    let cpu_column = inspected
        .sample_types
        .iter()
        .enumerate()
        .find_map(|(index, (name, unit))| {
            let scale = pprof::time_scale_ns(unit)?;
            name.eq_ignore_ascii_case("cpu").then_some((index, scale))
        });
    let mut tracks: HashMap<(u8, String), u32> = HashMap::new();
    let mut cpu_ns = 0_u64;
    let mut saw_cpu = false;
    for sample in &inspected.samples {
        let value = sample
            .values
            .get(inspected.default_sample_type)
            .copied()
            .unwrap_or(0);
        if value < 0 {
            return Err(Error::invalid(
                "pprof",
                format!("negative sample value {value}"),
            ));
        }
        let weight = match scale {
            Some(scale) => pprof::ticks_from_f64(value as f64 * scale, "pprof")?,
            None => u64::try_from(value).unwrap_or(0),
        };
        if let Some((index, scale)) = cpu_column
            && let Some(cpu) = sample.values.get(index).copied()
        {
            if cpu < 0 {
                return Err(Error::invalid("pprof", format!("negative cpu value {cpu}")));
            }
            saw_cpu = true;
            cpu_ns = cpu_ns.saturating_add(pprof::ticks_from_f64(cpu as f64 * scale, "pprof")?);
        }
        let thread = if let Some(track) = &sample.track {
            let kind = match track.kind {
                PprofTrack::Thread => TrackKind::Thread,
                PprofTrack::Goroutine => TrackKind::Goroutine,
            };
            let key = (kind as u8, track.name.clone());
            if let Some(id) = tracks.get(&key) {
                *id
            } else {
                let id = trace.add_thread(track.name.clone(), kind);
                tracks.insert(key, id);
                id
            }
        } else if let Some(id) = tracks.get(&(TrackKind::Profile as u8, "Samples".to_string())) {
            *id
        } else if tracks.is_empty() {
            let id = trace.add_thread("Samples", TrackKind::Profile);
            tracks.insert((TrackKind::Profile as u8, "Samples".to_string()), id);
            id
        } else {
            let id = trace.add_thread("unlabeled", TrackKind::Profile);
            tracks.insert((TrackKind::Profile as u8, "unlabeled".to_string()), id);
            id
        };
        let mut frames = Vec::with_capacity(sample.frames.len());
        for frame in sample.frames.iter().rev() {
            frames.push(trace.intern_normalized(ViewFrame {
                name: frame.name.clone(),
                file: frame.file.clone(),
                line: frame.line,
                column: frame.column,
                runtime: frame.runtime,
                module: frame.module.clone(),
                address: frame.address,
                system: frame.system,
            }));
        }
        trace.add_sample(thread, &frames, 0, weight)?;
    }
    if saw_cpu && cpu_ns > 0 {
        trace.cpu_ns = Some(cpu_ns);
    }
    if tracks.len() <= 1
        && trace
            .threads
            .iter()
            .all(|thread| thread.kind == TrackKind::Profile)
    {
        trace.notes.push(
            "This pprof profile does not label threads. Samples stay in one track.".to_string(),
        );
    }
    if trace.cpu_ns.is_none()
        && matches!(trace.weight_kind, WeightKind::Count)
        && (trace.wall_ns.is_some() || trace.period_ns.is_some())
    {
        trace.notes.push(
            "Weights are sample counts. Wall duration and the sampling period are metadata from the file. They are not a CPU utilization curve.".to_string(),
        );
    }
    Ok(trace)
}

fn from_cpuprofile(value: &Value) -> Result<Trace, Error> {
    let root = if value.get("nodes").is_some() {
        value
    } else if value.pointer("/profile/nodes").is_some() {
        &value["profile"]
    } else {
        return Err(Error::invalid("cpuprofile", "missing nodes array"));
    };
    let nodes = root
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("cpuprofile", "nodes is not an array"))?;
    let mut parsed: HashMap<u64, CpuNode> = HashMap::new();
    let mut children = Vec::new();
    for node in nodes {
        let id = json_u64(node.get("id"))
            .ok_or_else(|| Error::invalid("cpuprofile", "node is missing id"))?;
        let call = node.get("callFrame").ok_or_else(|| {
            Error::invalid("cpuprofile", format!("node {id} is missing callFrame"))
        })?;
        let (runtime, name) = call_name(call);
        let url = call.get("url").and_then(Value::as_str).unwrap_or("");
        let file = if url.is_empty() {
            None
        } else {
            Some(url.to_string())
        };
        let hit = json_u64(node.get("hitCount")).unwrap_or(0);
        let child_ids = node
            .get("children")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| json_u64(Some(value)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        children.push((id, child_ids));
        parsed.insert(
            id,
            CpuNode {
                runtime,
                name,
                file,
                line: source_position(call.get("lineNumber")),
                column: source_position(call.get("columnNumber")),
                parent: None,
                hit,
                is_root: false,
            },
        );
    }
    for (parent, child_ids) in children {
        for child in child_ids {
            if let Some(node) = parsed.get_mut(&child) {
                node.parent = Some(parent);
            }
        }
    }
    for node in parsed.values_mut() {
        node.is_root = node.name == "(root)";
    }

    let mut trace = Trace::new("cpuprofile", WeightKind::Count, "count");
    let thread = trace.add_thread("Samples", TrackKind::Profile);
    trace
        .notes
        .push("This CPU profile does not record a thread id. Samples are one track.".to_string());
    let mut ids = HashMap::new();
    let samples = root.get("samples").and_then(Value::as_array);
    if let Some(samples) = samples.filter(|samples| !samples.is_empty()) {
        let deltas = root.get("timeDeltas").and_then(Value::as_array);
        let aligned = deltas.is_some_and(|deltas| deltas.len() == samples.len());
        let repeats = sample_ids_repeat(samples);
        // scriptId 0 and startTime 0 is the shape `convert` and non-V8
        // `--format cpuprofile` write. A repeated sample id in that shape is
        // the same folded stack stored twice, not a clock.
        let folded = aligned && anyprof_folded_cpuprofile(root);
        if aligned && repeats && !folded {
            // V8's timeDeltas are microseconds between observations. A repeated
            // node id is a stack seen at more than one time, which folded
            // exports do not retain.
            trace.weight_kind = WeightKind::SampledTime;
            trace.unit = "microseconds".to_string();
            let mut cursor = 0_u64;
            let mut adjusted = 0_u64;
            for (index, sample) in samples.iter().enumerate() {
                let id = json_u64(Some(sample)).ok_or_else(|| {
                    Error::invalid("cpuprofile", format!("sample {index} is not an id"))
                })?;
                let delta = json_i64(deltas.and_then(|deltas| deltas.get(index))).unwrap_or(0);
                let micros = if delta > 0 {
                    u64::try_from(delta).unwrap_or(u64::MAX)
                } else {
                    adjusted = adjusted.saturating_add(1);
                    1
                };
                let weight = micros.saturating_mul(1_000);
                let frames = cpu_stack(&parsed, &mut ids, &mut trace, id)?;
                trace.add_sample(thread, &frames, cursor, weight)?;
                cursor = cursor.saturating_add(weight);
            }
            trace.timeline = Some((0, cursor));
            if adjusted > 0 {
                trace.notes.push(format!(
                    "{adjusted} non-positive timeDeltas were counted as 1 µs so later samples stayed aligned"
                ));
            }
        } else if folded {
            trace.timeline_disabled = Some(
                "This CPU profile has scriptId 0 on every frame and startTime 0. Its timeDeltas are folded sample counts, not a clock, so there is no timeline.".to_string(),
            );
            for (index, sample) in samples.iter().enumerate() {
                let id = json_u64(Some(sample)).ok_or_else(|| {
                    Error::invalid("cpuprofile", format!("sample {index} is not an id"))
                })?;
                let delta = json_i64(deltas.and_then(|deltas| deltas.get(index)))
                    .filter(|delta| *delta > 0)
                    .and_then(|delta| u64::try_from(delta).ok())
                    .unwrap_or(1);
                let frames = cpu_stack(&parsed, &mut ids, &mut trace, id)?;
                trace.add_sample(thread, &frames, 0, delta)?;
            }
        } else if aligned {
            trace.weight_kind = WeightKind::Duration;
            trace.unit = "microseconds".to_string();
            trace.timeline_disabled = Some(
                "Each stack is stored once, so this CPU profile is an aggregate of timeDeltas. Sample order is not a timeline.".to_string(),
            );
            for (index, sample) in samples.iter().enumerate() {
                let id = json_u64(Some(sample)).ok_or_else(|| {
                    Error::invalid("cpuprofile", format!("sample {index} is not an id"))
                })?;
                let delta = json_i64(deltas.and_then(|deltas| deltas.get(index)))
                    .filter(|delta| *delta > 0)
                    .map(|delta| {
                        u64::try_from(delta)
                            .unwrap_or(u64::MAX)
                            .saturating_mul(1_000)
                    })
                    .unwrap_or(1_000);
                let frames = cpu_stack(&parsed, &mut ids, &mut trace, id)?;
                trace.add_sample(thread, &frames, 0, delta)?;
            }
        } else {
            trace.timeline_disabled = Some(
                "timeDeltas does not have one entry per sample, so this profile is shown as hit counts and has no timeline.".to_string(),
            );
            for (index, sample) in samples.iter().enumerate() {
                let id = json_u64(Some(sample)).ok_or_else(|| {
                    Error::invalid("cpuprofile", format!("sample {index} is not an id"))
                })?;
                let delta = deltas
                    .and_then(|deltas| deltas.get(index))
                    .and_then(|delta| json_i64(Some(delta)))
                    .filter(|delta| *delta > 0)
                    .and_then(|delta| u64::try_from(delta).ok())
                    .unwrap_or(1);
                let frames = cpu_stack(&parsed, &mut ids, &mut trace, id)?;
                trace.add_sample(thread, &frames, 0, delta)?;
            }
        }
    } else {
        trace.timeline_disabled = Some(
            "This CPU profile only has per-node hit counts, not sample times, so there is no timeline.".to_string(),
        );
        for id in parsed.keys().copied().collect::<Vec<_>>() {
            let hit = parsed[&id].hit;
            if hit == 0 || parsed[&id].is_root {
                continue;
            }
            let frames = cpu_stack(&parsed, &mut ids, &mut trace, id)?;
            trace.add_sample(thread, &frames, 0, hit)?;
        }
    }
    Ok(trace)
}

struct CpuNode {
    runtime: Runtime,
    name: String,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    parent: Option<u64>,
    hit: u64,
    is_root: bool,
}

fn cpu_stack(
    parsed: &HashMap<u64, CpuNode>,
    ids: &mut HashMap<u64, u32>,
    trace: &mut Trace,
    mut id: u64,
) -> Result<Vec<u32>, Error> {
    let mut leaf_to_root = Vec::new();
    let mut seen = HashSet::new();
    for _ in 0..MAX_DEPTH {
        if !seen.insert(id) {
            return Err(Error::invalid(
                "cpuprofile",
                format!("node {id} is part of a parent cycle"),
            ));
        }
        let Some(node) = parsed.get(&id) else {
            return Err(Error::invalid(
                "cpuprofile",
                format!("sample references unknown node {id}"),
            ));
        };
        if !node.is_root {
            let frame = if let Some(frame) = ids.get(&id) {
                *frame
            } else {
                let frame = trace.intern_normalized(ViewFrame {
                    name: node.name.clone(),
                    file: node.file.clone(),
                    line: node.line,
                    column: node.column,
                    runtime: node.runtime,
                    module: None,
                    address: None,
                    system: false,
                });
                ids.insert(id, frame);
                frame
            };
            leaf_to_root.push(frame);
        }
        match node.parent {
            Some(parent) => id = parent,
            None => {
                leaf_to_root.reverse();
                return Ok(leaf_to_root);
            }
        }
    }
    Err(Error::invalid(
        "cpuprofile",
        format!("node parent chain is deeper than {MAX_DEPTH} frames"),
    ))
}

/// A node id that occurs twice was stored more than once. In a V8 profile
/// that is two observations. In an anyprof rebuild it is still one folded stack.
fn sample_ids_repeat(samples: &[Value]) -> bool {
    let mut seen = HashSet::new();
    for sample in samples {
        let Some(id) = json_u64(Some(sample)) else {
            continue;
        };
        if !seen.insert(id) {
            return true;
        }
    }
    false
}

/// `anyprof convert` and non-V8 `--format cpuprofile` write this shape:
/// `scriptId` `"0"` on every frame and `startTime` 0. Sample ids may repeat
/// when the same folded stack was stored more than once. That is not a clock.
fn anyprof_folded_cpuprofile(root: &Value) -> bool {
    if json_i64(root.get("startTime")).unwrap_or(0) != 0 {
        return false;
    }
    let Some(nodes) = root.get("nodes").and_then(Value::as_array) else {
        return false;
    };
    !nodes.is_empty()
        && nodes
            .iter()
            .all(|node| match node.pointer("/callFrame/scriptId") {
                Some(Value::String(text)) => text == "0",
                Some(Value::Number(number)) => number.as_u64() == Some(0),
                _ => false,
            })
}

fn call_name(call: &Value) -> (Runtime, String) {
    let raw = call
        .get("functionName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (runtime, name) = match split_runtime_prefix(raw) {
        Some((runtime, name)) => (runtime, name),
        None => (Runtime::Node, raw),
    };
    let name = if name.is_empty() {
        "(anonymous)".to_string()
    } else {
        name.to_string()
    };
    (runtime, name)
}

fn source_position(value: Option<&Value>) -> Option<u32> {
    let number = json_i64(value)?;
    if number < 0 {
        None
    } else {
        u32::try_from(number)
            .ok()
            .map(|number| number.saturating_add(1))
    }
}

fn from_speedscope(value: &Value) -> Result<Trace, Error> {
    let shared = value
        .pointer("/shared/frames")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("speedscope", "shared.frames is missing"))?;
    let profiles = value
        .get("profiles")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("speedscope", "profiles is missing"))?;
    if profiles.is_empty() {
        return Err(Error::invalid("speedscope", "profiles is empty"));
    }
    let mut classes = Vec::new();
    for profile in profiles {
        let unit = profile
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("none");
        classes.push(unit_class(unit)?);
    }
    let class = classes[0].clone();
    if classes.iter().any(|other| !other.same_family(&class)) {
        return Err(Error::invalid(
            "speedscope",
            "profiles do not share a time, count, or byte unit",
        ));
    }
    let (kind, unit_name, timed) = match &class {
        UnitClass::Time(_) => (WeightKind::SampledTime, "nanoseconds".to_string(), true),
        UnitClass::Count => (WeightKind::Count, "count".to_string(), false),
        UnitClass::Bytes => (WeightKind::Bytes, "bytes".to_string(), false),
        UnitClass::Other(name) => (WeightKind::Other, name.clone(), false),
    };
    let mut trace = Trace::new("speedscope", kind, unit_name);
    if !timed {
        let unit = profiles[0]
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("none");
        trace.timeline_disabled = Some(format!(
            "This speedscope profile uses unit {unit}. Stacks are weighted in file order, which is not a clock, and thread ids were not recorded."
        ));
    }
    let mut axis_start = u64::MAX;
    let mut axis_end = 0_u64;
    let mut saw_events = false;
    let mut saw_repeat = false;
    for (index, profile) in profiles.iter().enumerate() {
        let scale = match &classes[index] {
            UnitClass::Time(scale) => *scale,
            _ => 1.0,
        };
        let name = profile
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("Profile {}", index + 1));
        let thread = trace.add_thread(name, TrackKind::Profile);
        let profile_kind = profile.get("type").and_then(Value::as_str).unwrap_or("");
        let start = json_f64(profile.get("startValue")).unwrap_or(0.0);
        if start < 0.0 {
            return Err(Error::invalid("speedscope", "startValue is negative"));
        }
        let start_ticks = pprof::ticks_from_f64(start * scale, "speedscope")?;
        let end_ticks = match json_f64(profile.get("endValue")) {
            Some(end) if end < 0.0 => {
                return Err(Error::invalid("speedscope", "endValue is negative"));
            }
            Some(end) => pprof::ticks_from_f64(end * scale, "speedscope")?,
            None => start_ticks,
        };
        let mut cursor = start_ticks;
        match profile_kind {
            "sampled" => {
                let Some(samples) = profile.get("samples").and_then(Value::as_array) else {
                    axis_start = axis_start.min(start_ticks);
                    axis_end = axis_end.max(end_ticks.max(start_ticks));
                    continue;
                };
                let weights = profile.get("weights").and_then(Value::as_array);
                if let Some(weights) = weights
                    && weights.len() != samples.len()
                {
                    return Err(Error::invalid(
                        "speedscope",
                        format!(
                            "profile {index} has {} samples and {} weights",
                            samples.len(),
                            weights.len()
                        ),
                    ));
                }
                let mut seen_stacks = HashSet::new();
                for (sample_index, sample) in samples.iter().enumerate() {
                    let stack = sample.as_array().ok_or_else(|| {
                        Error::invalid(
                            "speedscope",
                            format!("sample {sample_index} is not a stack"),
                        )
                    })?;
                    let weight_value = match weights.and_then(|weights| weights.get(sample_index)) {
                        Some(weight) => json_f64(Some(weight)).ok_or_else(|| {
                            Error::invalid(
                                "speedscope",
                                format!("profile {index} weight {sample_index} is not a number"),
                            )
                        })?,
                        None => 1.0,
                    };
                    if weight_value < 0.0 {
                        return Err(Error::invalid(
                            "speedscope",
                            format!("negative weight {weight_value}"),
                        ));
                    }
                    let weight = pprof::ticks_from_f64(weight_value * scale, "speedscope")?;
                    let mut frames = Vec::with_capacity(stack.len());
                    for frame_index in stack {
                        let frame_index = json_u64(Some(frame_index)).ok_or_else(|| {
                            Error::invalid("speedscope", "frame index is not an integer")
                        })?;
                        frames.push(speed_frame(&mut trace, shared, frame_index)?);
                    }
                    if !seen_stacks.insert(frames.clone()) {
                        saw_repeat = true;
                    }
                    trace.add_sample(thread, &frames, cursor, weight)?;
                    cursor = cursor.saturating_add(weight);
                }
            }
            "evented" => {
                saw_events = true;
                let events = profile
                    .get("events")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut ordered: Vec<(u64, usize, String, u64)> = Vec::with_capacity(events.len());
                for (event_index, event) in events.iter().enumerate() {
                    let at = json_f64(event.get("at")).ok_or_else(|| {
                        Error::invalid(
                            "speedscope",
                            format!("profile {index} event {event_index} has no timestamp"),
                        )
                    })?;
                    if at < 0.0 {
                        return Err(Error::invalid("speedscope", "event time is negative"));
                    }
                    let kind = event
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let frame = json_u64(event.get("frame")).ok_or_else(|| {
                        Error::invalid(
                            "speedscope",
                            format!("profile {index} event {event_index} has no frame"),
                        )
                    })?;
                    ordered.push((
                        pprof::ticks_from_f64(at * scale, "speedscope")?,
                        event_index,
                        kind,
                        frame,
                    ));
                }
                ordered.sort_by_key(|(at, event_index, _, _)| (*at, *event_index));
                let mut open = Vec::new();
                for (at, _, kind, frame_index) in ordered {
                    if at < cursor {
                        return Err(Error::invalid(
                            "speedscope",
                            "event timestamps go backwards",
                        ));
                    }
                    if at > cursor && !open.is_empty() {
                        trace.add_sample(thread, &open, cursor, at - cursor)?;
                    }
                    cursor = at;
                    let frame = speed_frame(&mut trace, shared, frame_index)?;
                    match kind.as_str() {
                        "O" | "o" => open.push(frame),
                        "C" | "c" => {
                            if open.last().copied() != Some(frame) {
                                let open_name = open
                                    .last()
                                    .map(|id| trace.frames[*id as usize].name.clone())
                                    .unwrap_or_else(|| "(empty)".to_string());
                                return Err(Error::invalid(
                                    "speedscope",
                                    format!(
                                        "close event for frame {frame_index} did not match the open frame {open_name}"
                                    ),
                                ));
                            }
                            open.pop();
                        }
                        other => {
                            return Err(Error::invalid(
                                "speedscope",
                                format!("unknown event type {other}"),
                            ));
                        }
                    }
                }
                let end = end_ticks.max(cursor);
                if end > cursor && !open.is_empty() {
                    trace.add_sample(thread, &open, cursor, end - cursor)?;
                    cursor = end;
                }
            }
            other => {
                return Err(Error::invalid(
                    "speedscope",
                    format!("unknown profile type {other}"),
                ));
            }
        }
        axis_start = axis_start.min(start_ticks);
        axis_end = axis_end.max(end_ticks.max(cursor));
    }
    if timed && (saw_events || saw_repeat) {
        if axis_start == u64::MAX {
            axis_start = 0;
        }
        trace.timeline = Some((axis_start, axis_end.max(axis_start)));
    } else if timed {
        // A time unit with one row per stack is an aggregate. Laying those
        // rows out in file order would invent a chronology the recorder folded
        // away. anyprof's own speedscope uses unit `none` and never gets here.
        trace.weight_kind = WeightKind::Duration;
        trace.timeline = None;
        trace.timeline_disabled = Some(
            "Each stack is stored once. Sample order is the file order of folded weights, not a clock, so there is no timeline.".to_string(),
        );
    }
    if profiles.len() > 1 {
        trace.notes.push(
            "Tracks are the profiles listed in the file. They are not thread ids recovered from sample order.".to_string(),
        );
    } else if trace.threads.len() == 1 {
        trace.notes.push(
            "This speedscope file does not record thread ids. Samples stay in one track."
                .to_string(),
        );
    }
    Ok(trace)
}

fn speed_frame(trace: &mut Trace, frames: &[Value], index: u64) -> Result<u32, Error> {
    let frame = frames.get(index as usize).ok_or_else(|| {
        Error::invalid("speedscope", format!("frame index {index} is out of range"))
    })?;
    let name = frame.get("name").and_then(Value::as_str).unwrap_or("");
    let (runtime, parsed) = split_runtime_prefix(name).unwrap_or((Runtime::Native, name));
    let file = frame
        .get("file")
        .and_then(Value::as_str)
        .filter(|file| !file.is_empty())
        .map(str::to_string);
    let line = json_u64(frame.get("line")).and_then(|line| u32::try_from(line).ok());
    let column = json_u64(frame.get("col")).and_then(|column| u32::try_from(column).ok());
    Ok(trace.intern_normalized(ViewFrame {
        name: parsed.to_string(),
        file,
        line,
        column,
        runtime,
        module: None,
        address: None,
        system: false,
    }))
}

#[derive(Clone)]
enum UnitClass {
    Time(f64),
    Count,
    Bytes,
    Other(String),
}

impl UnitClass {
    fn same_family(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Time(_), Self::Time(_))
                | (Self::Count, Self::Count)
                | (Self::Bytes, Self::Bytes)
                | (Self::Other(_), Self::Other(_))
        )
    }
}

fn unit_class(unit: &str) -> Result<UnitClass, Error> {
    if let Some(scale) = pprof::time_scale_ns(unit) {
        return Ok(UnitClass::Time(scale));
    }
    match unit.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "count" | "samples" => Ok(UnitClass::Count),
        "bytes" | "byte" => Ok(UnitClass::Bytes),
        other => Ok(UnitClass::Other(other.to_string())),
    }
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_i64() {
        return u64::try_from(number).ok();
    }
    let number = value.as_f64()?;
    if number.is_finite() && number >= 0.0 && number <= u64::MAX as f64 {
        Some(number.round() as u64)
    } else {
        None
    }
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    let number = value.as_f64()?;
    if number.is_finite() && number >= i64::MIN as f64 && number <= i64::MAX as f64 {
        Some(number.round() as i64)
    } else {
        None
    }
}

fn json_f64(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    number.is_finite().then_some(number)
}

fn brief(err: &impl std::fmt::Display) -> String {
    let text = err.to_string();
    if text.len() > 240 {
        format!("{}…", &text[..240])
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::{Query, WeightKind, activity, aggregate, load_trace};
    use crate::proto::Writer;
    use crate::{Format, export};

    fn names<'a>(trace: &'a super::Trace, frames: &[u32]) -> Vec<&'a str> {
        frames
            .iter()
            .map(|id| trace.frames[*id as usize].name.as_str())
            .collect()
    }

    fn frame_named<'a>(trace: &'a super::Trace, name: &str) -> &'a super::ViewFrame {
        trace
            .frames
            .iter()
            .find(|frame| frame.name == name)
            .unwrap_or_else(|| panic!("missing frame {name}"))
    }

    fn genuine_cpuprofile() -> &'static str {
        r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "1", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": [2, 3]},
            {"id": 2, "callFrame": {"functionName": "hot", "scriptId": "7", "url": "app.js", "lineNumber": 9, "columnNumber": 0}, "children": []},
            {"id": 3, "callFrame": {"functionName": "cold", "scriptId": "7", "url": "app.js", "lineNumber": 20, "columnNumber": 2}, "children": []}
          ],
          "startTime": 5000,
          "endTime": 8000,
          "samples": [2, 2, 3],
          "timeDeltas": [1000, 1000, 500]
        }"#
    }

    #[test]
    fn anyprof_exports_stay_aggregates() {
        let profile = crate::fixture();
        let speedscope = export(&profile, Format::Speedscope).unwrap();
        let trace = load_trace(&speedscope).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Count);
        assert_eq!(trace.total_weight(), profile.total_weight());
        assert_eq!(trace.threads.len(), 1);
        assert_eq!(trace.threads[0].kind, super::TrackKind::Profile);
        assert!(activity(&trace, 0, 1, 4).is_err());
        let disabled = trace.timeline_disabled.unwrap();
        assert!(disabled.contains("not a clock"), "{disabled}");

        let pprof = export(&profile, Format::Pprof).unwrap();
        let trace = load_trace(&pprof).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Count);
        assert_eq!(trace.cpu_ns, None);
        assert_eq!(trace.wall_ns, Some(5_000_000_000));
        assert_eq!(trace.period_ns, Some(10_000_000));
        assert_eq!(trace.total_weight(), 7);
        assert!(
            trace
                .threads
                .iter()
                .all(|thread| thread.kind == super::TrackKind::Profile)
        );
        assert!(
            trace
                .notes
                .iter()
                .any(|note| note.contains("not a CPU utilization curve")),
            "{:?}",
            trace.notes
        );
        let hidden = aggregate(
            &trace,
            &Query {
                hide_system: true,
                ..Query::default()
            },
        );
        assert!(
            hidden
                .heaviest
                .iter()
                .all(|frame| trace.frames[*frame as usize].name != "schedule")
        );

        let converted = export(&profile, Format::Cpuprofile).unwrap();
        let trace = load_trace(&converted).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Count);
        assert_eq!(trace.total_weight(), profile.total_weight());
        assert_eq!(trace.cpu_ns, None);
        let disabled = trace.timeline_disabled.unwrap();
        assert!(disabled.contains("folded sample counts"), "{disabled}");

        let collapsed = export(&profile, Format::Collapsed).unwrap();
        let trace = load_trace(&collapsed).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.total_weight(), profile.total_weight());
        assert_eq!(trace.threads.len(), 1);
    }

    #[test]
    fn raw_cpuprofile_keeps_its_time_series() {
        let mut profile = crate::fixture();
        let raw = genuine_cpuprofile();
        profile.raw_cpuprofile = Some(raw.as_bytes().to_vec());
        let bytes = export(&profile, Format::Cpuprofile).unwrap();
        assert_eq!(bytes, raw.as_bytes());
        let trace = load_trace(&bytes).unwrap();
        assert_eq!(trace.weight_kind, WeightKind::SampledTime);
        assert_eq!(trace.timeline, Some((0, 2_500_000)));
        assert_eq!(trace.cpu_ns, None);
        assert_eq!(trace.threads.len(), 1);
        assert_eq!(trace.threads[0].kind, super::TrackKind::Profile);
        let hot = frame_named(&trace, "hot");
        assert_eq!(hot.file.as_deref(), Some("app.js"));
        assert_eq!(hot.line, Some(10));
        assert_eq!(hot.column, Some(1));

        let partial = aggregate(
            &trace,
            &Query {
                start: Some(1_500_000),
                end: Some(2_000_000),
                ..Query::default()
            },
        );
        assert_eq!(partial.total, 500_000);
        assert_eq!(names(&trace, &partial.heaviest), vec!["hot"]);
    }

    #[test]
    fn folded_cpuprofile_shapes_do_not_become_a_clock() {
        let folded = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": [2]},
            {"id": 2, "callFrame": {"functionName": "work", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": []}
          ],
          "startTime": 0,
          "endTime": 4,
          "samples": [2],
          "timeDeltas": [4]
        }"#;
        let trace = load_trace(folded.as_bytes()).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Count);
        assert_eq!(trace.total_weight(), 4);

        let unique_time = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "main", "scriptId": "9", "url": "a.js", "lineNumber": 0, "columnNumber": 0}, "children": [2, 3]},
            {"id": 2, "callFrame": {"functionName": "left", "scriptId": "9", "url": "a.js", "lineNumber": 1, "columnNumber": 0}, "children": []},
            {"id": 3, "callFrame": {"functionName": "right", "scriptId": "9", "url": "a.js", "lineNumber": 2, "columnNumber": 0}, "children": []}
          ],
          "startTime": 0,
          "samples": [2, 3],
          "timeDeltas": [4, 6]
        }"#;
        let trace = load_trace(unique_time.as_bytes()).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Duration);
        assert_eq!(trace.total_weight(), 10_000);

        let repeated_native = r#"{
          "nodes": [
            {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": [2]},
            {"id": 2, "callFrame": {"functionName": "work", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": []}
          ],
          "startTime": 0,
          "samples": [2, 2],
          "timeDeltas": [1000, -5]
        }"#;
        let trace = load_trace(repeated_native.as_bytes()).unwrap();
        assert!(trace.timeline.is_none(), "{:?}", trace.timeline);
        assert_eq!(trace.weight_kind, WeightKind::Count);
        assert_eq!(trace.total_weight(), 1_001);
        assert!(
            trace
                .timeline_disabled
                .as_deref()
                .is_some_and(|note| note.contains("folded sample counts")),
            "{:?}",
            trace.timeline_disabled
        );
    }

    #[test]
    fn speedscope_order_is_a_timeline_only_when_the_series_repeats() {
        let folded = r#"{
          "shared": {"frames": [{"name": "main"}, {"name": "hot"}]},
          "profiles": [{
            "type": "sampled",
            "name": "python",
            "unit": "none",
            "startValue": 0,
            "endValue": 10,
            "samples": [[0], [0, 1], [0]],
            "weights": [1, 2, 7]
          }]
        }"#;
        let trace = load_trace(folded.as_bytes()).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Count);
        assert_eq!(trace.total_weight(), 10);
        assert_eq!(trace.threads[0].kind, super::TrackKind::Profile);

        let unique = r#"{
          "shared": {"frames": [{"name": "main"}, {"name": "hot"}]},
          "profiles": [{
            "type": "sampled",
            "name": "t",
            "unit": "milliseconds",
            "startValue": 0,
            "endValue": 15,
            "samples": [[0, 1], [0]],
            "weights": [10, 5]
          }]
        }"#;
        let trace = load_trace(unique.as_bytes()).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.weight_kind, WeightKind::Duration);
        assert_eq!(trace.total_weight(), 15_000_000);

        let series = r#"{
          "shared": {"frames": [{"name": "main"}, {"name": "hot"}]},
          "profiles": [{
            "type": "sampled",
            "name": "t",
            "unit": "milliseconds",
            "startValue": 0,
            "endValue": 30,
            "samples": [[0, 1], [0, 1], [0]],
            "weights": [10, 10, 10]
          }]
        }"#;
        let trace = load_trace(series.as_bytes()).unwrap();
        assert_eq!(trace.timeline, Some((0, 30_000_000)));
        assert_eq!(trace.weight_kind, WeightKind::SampledTime);
        let buckets = activity(&trace, 0, 30_000_000, 3).unwrap();
        assert_eq!(buckets.rows.len(), 1);
        assert_eq!(buckets.rows[0].iter().sum::<u64>(), 30_000_000);
    }

    #[test]
    fn evented_speedscope_uses_open_and_close_times() {
        let body = r#"{
          "shared": {"frames": [{"name": "main"}, {"name": "hot"}]},
          "profiles": [{
            "type": "evented",
            "name": "worker",
            "unit": "nanoseconds",
            "startValue": 0,
            "endValue": 100,
            "events": [
              {"type": "O", "frame": 0, "at": 0},
              {"type": "O", "frame": 1, "at": 10},
              {"type": "C", "frame": 1, "at": 40},
              {"type": "C", "frame": 0, "at": 100}
            ]
          }]
        }"#;
        let trace = load_trace(body.as_bytes()).unwrap();
        assert_eq!(trace.timeline, Some((0, 100)));
        let partial = aggregate(
            &trace,
            &Query {
                start: Some(20),
                end: Some(50),
                ..Query::default()
            },
        );
        assert_eq!(partial.total, 30);
        let hot = partial
            .functions
            .iter()
            .find(|row| trace.frames[row.frame as usize].name == "hot")
            .unwrap();
        assert_eq!(hot.weight, 20);
        assert_eq!(hot.self_weight, 20);
        let main = partial
            .functions
            .iter()
            .find(|row| trace.frames[row.frame as usize].name == "main")
            .unwrap();
        assert_eq!(main.weight, 30);
        assert_eq!(main.self_weight, 10);
    }

    #[test]
    fn recursion_invert_and_self_weight() {
        let trace = load_trace(b"a;b;a 10\n").unwrap();
        let full = aggregate(&trace, &Query::default());
        assert_eq!(full.total, 10);
        assert_eq!(names(&trace, &full.heaviest), vec!["a", "b", "a"]);
        let outer = &full.nodes[0];
        assert_eq!(trace.frames[outer.frame as usize].name, "a");
        assert_eq!(outer.weight, 10);
        assert_eq!(outer.self_weight, 0);
        let leaf = full.nodes.last().unwrap();
        assert_eq!(trace.frames[leaf.frame as usize].name, "a");
        assert_eq!(leaf.self_weight, 10);
        let function = full
            .functions
            .iter()
            .find(|row| trace.frames[row.frame as usize].name == "a")
            .unwrap();
        assert_eq!(function.weight, 10);
        assert_eq!(function.self_weight, 10);

        let inverted = aggregate(
            &trace,
            &Query {
                invert: true,
                ..Query::default()
            },
        );
        assert_eq!(names(&trace, &inverted.heaviest), vec!["a", "b", "a"]);
        assert_eq!(inverted.nodes[0].self_weight, 10);
        assert_eq!(inverted.nodes[0].weight, 10);
        assert_eq!(inverted.nodes.last().unwrap().self_weight, 0);
    }

    #[test]
    fn system_frames_come_only_from_kernel_or_a_mapped_system_library() {
        let bytes = labeled_pprof();
        let trace = load_trace(&bytes).unwrap();
        assert!(trace.timeline.is_none());
        assert_eq!(trace.cpu_ns, Some(7_500_000));
        assert_eq!(trace.wall_ns, Some(5_000_000_000));
        assert_eq!(trace.period_ns, Some(10_000_000));
        assert_eq!(trace.threads.len(), 2);
        assert_eq!(trace.threads[0].kind, super::TrackKind::Thread);
        assert_eq!(trace.threads[0].name, "worker");
        assert_eq!(trace.threads[1].name, "other");
        assert!(!frame_named(&trace, "std::vec::Vec::push").system);
        assert!(frame_named(&trace, "read").system);
        assert!(activity(&trace, 0, 1, 4).is_err());

        let all = aggregate(&trace, &Query::default());
        assert_eq!(all.total, 16);
        let hidden = aggregate(
            &trace,
            &Query {
                hide_system: true,
                ..Query::default()
            },
        );
        assert_eq!(hidden.total, 15);
        assert!(
            hidden
                .nodes
                .iter()
                .all(|node| trace.frames[node.frame as usize].name != "read")
        );
        let other = aggregate(
            &trace,
            &Query {
                threads: Some(vec![1]),
                ..Query::default()
            },
        );
        assert_eq!(other.total, 2);
        assert_eq!(names(&trace, &other.heaviest), vec!["main"]);
    }

    #[test]
    fn collapsed_does_not_guess_system_frames_from_the_name() {
        let trace = load_trace(b"std::vec::push;app 5\nmain;[kernel] schedule 2\n").unwrap();
        assert!(!frame_named(&trace, "std::vec::push").system);
        assert!(frame_named(&trace, "schedule").system);
        let hidden = aggregate(
            &trace,
            &Query {
                hide_system: true,
                ..Query::default()
            },
        );
        assert_eq!(hidden.total, 7);
        assert!(
            hidden
                .nodes
                .iter()
                .any(|node| trace.frames[node.frame as usize].name == "std::vec::push")
        );
        assert!(
            hidden
                .nodes
                .iter()
                .all(|node| trace.frames[node.frame as usize].name != "schedule")
        );
    }

    #[test]
    fn empty_and_malformed_inputs_fail_clearly() {
        assert!(load_trace(b"").is_err());
        assert!(load_trace(b"{").is_err());
        assert!(load_trace(&[0x1f, 0x8b, 1, 2]).is_err());
        let empty = load_trace(b"{\"nodes\":[]}").unwrap();
        assert!(empty.timeline.is_none());
        assert_eq!(empty.total_weight(), 0);
    }

    #[test]
    fn aggregate_of_many_folded_samples_stays_bounded() {
        let mut patterns = Vec::with_capacity(64);
        for pattern in 0..64 {
            let mut parts = Vec::with_capacity(16);
            parts.push("main".to_string());
            for depth in 0..15 {
                parts.push(format!("f{pattern}_{depth}"));
            }
            patterns.push(parts.join(";"));
        }
        let mut body = String::with_capacity(200_000 * 48);
        for index in 0..200_000 {
            body.push_str(&patterns[index % 64]);
            body.push_str(" 1\n");
        }
        let started = std::time::Instant::now();
        let trace = load_trace(body.as_bytes()).unwrap();
        let loaded = started.elapsed();
        let started = std::time::Instant::now();
        let tree = aggregate(&trace, &Query::default());
        let aggregated = started.elapsed();
        assert!(trace.timeline.is_none());
        assert_eq!(tree.total, 200_000);
        assert!(
            loaded.as_secs() < 15 && aggregated.as_secs() < 5,
            "load {loaded:?} aggregate {aggregated:?}"
        );
    }

    fn labeled_pprof() -> Vec<u8> {
        let strings = [
            "",
            "samples",
            "count",
            "cpu",
            "nanoseconds",
            "thread",
            "worker",
            "main",
            "hot",
            "read",
            "/usr/lib/libc.so.6",
            "app.rs",
            "std::vec::Vec::push",
            "other",
        ];
        let mut message = Writer::new();
        message.field_message(1, &value_type(1, 2));
        message.field_message(1, &value_type(3, 4));
        let worker = label(5, 6);
        let other = label(5, 13);
        message.field_message(2, &sample(&[2, 1], &[4, 4_000_000], &worker));
        message.field_message(2, &sample(&[3], &[1, 1_000_000], &worker));
        message.field_message(2, &sample(&[4, 1], &[9, 500_000], &worker));
        message.field_message(2, &sample(&[1], &[2, 2_000_000], &other));
        message.field_message(3, &mapping(1, 10));
        message.field_message(4, &location(1, 0, 1, 10));
        message.field_message(4, &location(2, 0, 2, 20));
        message.field_message(4, &location(3, 1, 3, 1));
        message.field_message(4, &location(4, 0, 4, 30));
        message.field_message(5, &function(1, 7, 11));
        message.field_message(5, &function(2, 8, 11));
        message.field_message(5, &function(3, 9, 0));
        message.field_message(5, &function(4, 12, 11));
        for text in strings {
            message.field_string(6, text);
        }
        message.field_varint(10, 5_000_000_000);
        message.field_message(11, &value_type(3, 4));
        message.field_varint(12, 10_000_000);
        message.into_vec()
    }

    fn value_type(type_index: u64, unit_index: u64) -> Writer {
        let mut writer = Writer::new();
        writer.field_varint(1, type_index);
        writer.field_varint(2, unit_index);
        writer
    }

    fn function(id: u64, name: u64, file: u64) -> Writer {
        let mut writer = Writer::new();
        writer.field_varint(1, id);
        writer.field_varint(2, name);
        writer.field_varint(4, file);
        writer
    }

    fn location(id: u64, mapping_id: u64, function_id: u64, line: u64) -> Writer {
        let mut line_msg = Writer::new();
        line_msg.field_varint(1, function_id);
        line_msg.field_varint(2, line);
        let mut writer = Writer::new();
        writer.field_varint(1, id);
        writer.field_varint(2, mapping_id);
        writer.field_message(4, &line_msg);
        writer
    }

    fn mapping(id: u64, filename: u64) -> Writer {
        let mut writer = Writer::new();
        writer.field_varint(1, id);
        writer.field_varint(5, filename);
        writer
    }

    fn label(key: u64, text: u64) -> Writer {
        let mut writer = Writer::new();
        writer.field_varint(1, key);
        writer.field_varint(2, text);
        writer
    }

    fn sample(locations: &[u64], values: &[u64], label: &Writer) -> Writer {
        let mut writer = Writer::new();
        writer.field_packed_varints(1, locations);
        writer.field_packed_varints(2, values);
        writer.field_message(3, label);
        writer
    }
}
