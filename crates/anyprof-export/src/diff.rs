//! Differential profile aggregation across any two supported profile files.
//!
//! [`load_diff`] decodes both baseline (`before`) and comparison (`after`)
//! profiles via [`crate::load_trace`], reconciles compatible weight units,
//! unifies frames across ASLR addresses and line-number edits, and computes
//! [`DiffAggregate`] union call trees and flat function deltas.

use std::collections::{HashMap, HashSet};

use anyprof_core::Runtime;

use crate::error::Error;
use crate::trace::{Trace, ViewFrame, WeightKind, load_trace};

/// Two profiles unified onto a common frame table and weight scale.
#[derive(Debug, Clone)]
pub struct DiffTrace {
    /// Baseline profile ("before").
    pub before: Trace,
    /// Comparison profile ("after").
    pub after: Trace,
    /// Multiplier applied to `before` sample weights.
    pub before_scale: u64,
    /// Multiplier applied to `after` sample weights.
    pub after_scale: u64,
    /// Unified weight semantics.
    pub weight_kind: WeightKind,
    /// Unified unit token (`nanoseconds`, `count`, `bytes`, or custom).
    pub unit: String,
    /// Unified frames referenced by [`DiffTreeNode::frame`] and [`DiffFunctionRow::frame`].
    pub frames: Vec<ViewFrame>,
    /// Maps each index in `before.frames` to its unified index in [`Self::frames`].
    pub before_frame_map: Vec<u32>,
    /// Maps each index in `after.frames` to its unified index in [`Self::frames`].
    pub after_frame_map: Vec<u32>,
    /// Non-fatal notes about unit reconciliation or input caveats.
    pub notes: Vec<String>,
}

/// Filter options for [`diff_aggregate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffQuery {
    /// Put the leaf at the root of the differential call tree.
    pub invert: bool,
    /// Drop kernel frames and frames mapped from a system library.
    pub hide_system: bool,
}

/// Union call tree and function delta table for a [`DiffTrace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffAggregate {
    /// Total scaled weight in the baseline profile.
    pub baseline_total: u64,
    /// Total scaled weight in the comparison profile.
    pub comparison_total: u64,
    /// Sample records counted from the baseline profile.
    pub baseline_samples: u64,
    /// Sample records counted from the comparison profile.
    pub comparison_samples: u64,
    /// Sum of root `span` values for union flame-graph layout.
    pub span_total: u64,
    /// Preorder union call tree.
    pub nodes: Vec<DiffTreeNode>,
    /// Merged per-function baseline, comparison, and delta weights.
    pub functions: Vec<DiffFunctionRow>,
}

impl DiffAggregate {
    /// Signed total weight change (`comparison_total - baseline_total`).
    #[must_use]
    pub fn delta_total(&self) -> i128 {
        i128::from(self.comparison_total) - i128::from(self.baseline_total)
    }
}

/// One node in the differential call tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffTreeNode {
    /// Preorder index into [`DiffAggregate::nodes`].
    pub id: u32,
    /// Parent node index, or `None` for a root.
    pub parent: Option<u32>,
    /// Index into [`DiffTrace::frames`].
    pub frame: u32,
    /// Zero for a root.
    pub depth: u32,
    /// Inclusive weight in the baseline profile.
    pub baseline: u64,
    /// Inclusive weight in the comparison profile.
    pub comparison: u64,
    /// Self (leaf) weight in the baseline profile.
    pub baseline_self: u64,
    /// Self (leaf) weight in the comparison profile.
    pub comparison_self: u64,
    /// Bottom-up union layout width: `max(baseline_self, comparison_self) + sum(child.span)`.
    ///
    /// Guarantees children never overflow their parent while keeping 100%-eliminated
    /// baseline stacks visible in the differential flame graph.
    pub span: u64,
}

impl DiffTreeNode {
    /// Signed inclusive weight delta (`comparison - baseline`).
    #[must_use]
    pub fn delta_total(&self) -> i128 {
        i128::from(self.comparison) - i128::from(self.baseline)
    }

    /// Signed self weight delta (`comparison_self - baseline_self`).
    #[must_use]
    pub fn delta_self(&self) -> i128 {
        i128::from(self.comparison_self) - i128::from(self.baseline_self)
    }
}

/// Inclusive and self weights of one unified function across baseline and comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFunctionRow {
    /// Index into [`DiffTrace::frames`].
    pub frame: u32,
    /// Inclusive weight in the baseline profile.
    pub baseline: u64,
    /// Inclusive weight in the comparison profile.
    pub comparison: u64,
    /// Self (leaf) weight in the baseline profile.
    pub baseline_self: u64,
    /// Self (leaf) weight in the comparison profile.
    pub comparison_self: u64,
}

impl DiffFunctionRow {
    /// Signed inclusive weight delta (`comparison - baseline`).
    #[must_use]
    pub fn delta_total(&self) -> i128 {
        i128::from(self.comparison) - i128::from(self.baseline)
    }

    /// Signed self weight delta (`comparison_self - baseline_self`).
    #[must_use]
    pub fn delta_self(&self) -> i128 {
        i128::from(self.comparison_self) - i128::from(self.baseline_self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiffFrameKey {
    runtime: Runtime,
    name: String,
    file: Option<String>,
    module: Option<String>,
    system: bool,
}

impl DiffTrace {
    /// Whether any unified frame is classified as a system frame.
    #[must_use]
    pub fn has_system_frames(&self) -> bool {
        self.frames.iter().any(|frame| frame.system)
    }

    /// Reconcile two already-parsed [`Trace`]s into a [`DiffTrace`].
    pub fn from_traces(before: Trace, after: Trace) -> Result<Self, Error> {
        let mut notes = Vec::new();
        let (weight_kind, unit, before_scale, after_scale) =
            reconcile_units(&before, &after, &mut notes)?;

        let mut frames: Vec<ViewFrame> = Vec::new();
        let mut index: HashMap<DiffFrameKey, u32> = HashMap::new();

        // Intern `after` first so line numbers and addresses reflect the newer build
        // when source files were edited between runs.
        let mut after_frame_map = Vec::with_capacity(after.frames.len());
        for frame in &after.frames {
            let key = frame_key(frame);
            let id = if let Some(&existing) = index.get(&key) {
                merge_metadata(&mut frames[existing as usize], frame);
                existing
            } else {
                let id = frames.len() as u32;
                frames.push(frame.clone());
                index.insert(key, id);
                id
            };
            after_frame_map.push(id);
        }

        let mut before_frame_map = Vec::with_capacity(before.frames.len());
        for frame in &before.frames {
            let key = frame_key(frame);
            let id = if let Some(&existing) = index.get(&key) {
                merge_metadata(&mut frames[existing as usize], frame);
                existing
            } else {
                let id = frames.len() as u32;
                frames.push(frame.clone());
                index.insert(key, id);
                id
            };
            before_frame_map.push(id);
        }

        Ok(Self {
            before,
            after,
            before_scale,
            after_scale,
            weight_kind,
            unit,
            frames,
            before_frame_map,
            after_frame_map,
            notes,
        })
    }
}

/// Decode `before` and `after` profile bytes and unify them for differential analysis.
pub fn load_diff(before: &[u8], after: &[u8]) -> Result<DiffTrace, Error> {
    let before_trace =
        load_trace(before).map_err(|err| Error::invalid("diff", format!("before: {err}")))?;
    let after_trace =
        load_trace(after).map_err(|err| Error::invalid("diff", format!("after: {err}")))?;
    DiffTrace::from_traces(before_trace, after_trace)
}

fn is_time_kind(kind: WeightKind) -> bool {
    matches!(
        kind,
        WeightKind::SampledTime | WeightKind::CpuTime | WeightKind::Duration
    )
}

fn reconcile_units(
    before: &Trace,
    after: &Trace,
    notes: &mut Vec<String>,
) -> Result<(WeightKind, String, u64, u64), Error> {
    if is_time_kind(before.weight_kind) && is_time_kind(after.weight_kind) {
        let kind = if before.weight_kind == after.weight_kind {
            before.weight_kind
        } else {
            WeightKind::Duration
        };
        return Ok((kind, "nanoseconds".to_string(), 1, 1));
    }
    if is_time_kind(before.weight_kind)
        && after.weight_kind == WeightKind::Count
        && let Some(period) = after.period_ns.filter(|&p| p > 0)
    {
        notes.push(
            "Scaled comparison sample counts by its sampling period to compare durations."
                .to_string(),
        );
        return Ok((WeightKind::Duration, "nanoseconds".to_string(), 1, period));
    }
    if before.weight_kind == WeightKind::Count
        && is_time_kind(after.weight_kind)
        && let Some(period) = before.period_ns.filter(|&p| p > 0)
    {
        notes.push(
            "Scaled baseline sample counts by its sampling period to compare durations."
                .to_string(),
        );
        return Ok((WeightKind::Duration, "nanoseconds".to_string(), period, 1));
    }
    if before.weight_kind == WeightKind::Count && after.weight_kind == WeightKind::Count {
        return Ok((WeightKind::Count, "count".to_string(), 1, 1));
    }
    if before.weight_kind == WeightKind::Bytes && after.weight_kind == WeightKind::Bytes {
        return Ok((WeightKind::Bytes, "bytes".to_string(), 1, 1));
    }
    if before.weight_kind == WeightKind::Other
        && after.weight_kind == WeightKind::Other
        && before.unit.eq_ignore_ascii_case(&after.unit)
    {
        return Ok((WeightKind::Other, before.unit.clone(), 1, 1));
    }
    Err(Error::invalid(
        "diff",
        format!(
            "cannot compare incompatible weight units: baseline uses {} ({}) while comparison uses {} ({})",
            before.weight_kind.as_str(),
            before.unit,
            after.weight_kind.as_str(),
            after.unit
        ),
    ))
}

fn normalize_path_key(raw: &str) -> String {
    raw.trim().replace('\\', "/")
}

fn short_module_key(raw: &str) -> String {
    let norm = raw.trim().replace('\\', "/");
    norm.rsplit('/').next().unwrap_or(&norm).to_string()
}

fn frame_key(frame: &ViewFrame) -> DiffFrameKey {
    let file = frame
        .file
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(normalize_path_key);
    let module = if file.is_some() {
        None
    } else {
        frame
            .module
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(short_module_key)
    };
    DiffFrameKey {
        runtime: frame.runtime,
        name: frame.name.clone(),
        file,
        module,
        system: frame.system,
    }
}

fn merge_metadata(dst: &mut ViewFrame, candidate: &ViewFrame) {
    if dst.file.is_none() && candidate.file.is_some() {
        dst.file = candidate.file.clone();
    }
    if dst.line.is_none() && candidate.line.is_some() {
        dst.line = candidate.line;
    }
    if dst.column.is_none() && candidate.column.is_some() {
        dst.column = candidate.column;
    }
    if dst.module.is_none() && candidate.module.is_some() {
        dst.module = candidate.module.clone();
    }
}

struct BuildDiffNode {
    frame: u32,
    parent: Option<u32>,
    depth: u32,
    baseline: u64,
    comparison: u64,
    baseline_self: u64,
    comparison_self: u64,
    span: u64,
    children: Vec<u32>,
}

/// Build the differential union call tree and flat function table for `query`.
#[must_use]
pub fn diff_aggregate(diff: &DiffTrace, query: &DiffQuery) -> DiffAggregate {
    let mut nodes: Vec<BuildDiffNode> = Vec::new();
    let mut child: HashMap<(u32, u32), u32> = HashMap::new();
    let mut functions: HashMap<u32, (u64, u64, u64, u64)> = HashMap::new();
    let mut baseline_total = 0_u64;
    let mut comparison_total = 0_u64;
    let mut baseline_samples = 0_u64;
    let mut comparison_samples = 0_u64;
    let mut filtered = Vec::new();
    let mut seen = HashSet::new();

    let mut ingest =
        |raw_stack: &[u32], frame_map: &[u32], raw_weight: u64, scale: u64, is_after: bool| {
            let weight = raw_weight.saturating_mul(scale);
            if weight == 0 {
                return;
            }
            filtered.clear();
            for &raw_frame in raw_stack {
                let mapped = frame_map[raw_frame as usize];
                if query.hide_system && diff.frames[mapped as usize].system {
                    continue;
                }
                filtered.push(mapped);
            }
            if filtered.is_empty() {
                return;
            }
            if is_after {
                comparison_samples = comparison_samples.saturating_add(1);
                comparison_total = comparison_total.saturating_add(weight);
            } else {
                baseline_samples = baseline_samples.saturating_add(1);
                baseline_total = baseline_total.saturating_add(weight);
            }

            seen.clear();
            for &frame in &filtered {
                if seen.insert(frame) {
                    let entry = functions.entry(frame).or_insert((0, 0, 0, 0));
                    if is_after {
                        entry.1 = entry.1.saturating_add(weight);
                    } else {
                        entry.0 = entry.0.saturating_add(weight);
                    }
                }
            }
            let leaf = *filtered.last().expect("filtered stack is non-empty");
            let leaf_entry = functions.get_mut(&leaf).expect("leaf was inserted");
            if is_after {
                leaf_entry.3 = leaf_entry.3.saturating_add(weight);
            } else {
                leaf_entry.2 = leaf_entry.2.saturating_add(weight);
            }

            let self_at = if query.invert { 0 } else { filtered.len() - 1 };
            if query.invert {
                filtered.reverse();
            }
            let mut parent = u32::MAX;
            for (index, &frame) in filtered.iter().enumerate() {
                let id = if let Some(&id) = child.get(&(parent, frame)) {
                    id
                } else {
                    let id = nodes.len() as u32;
                    nodes.push(BuildDiffNode {
                        frame,
                        parent: if parent == u32::MAX {
                            None
                        } else {
                            Some(parent)
                        },
                        depth: index as u32,
                        baseline: 0,
                        comparison: 0,
                        baseline_self: 0,
                        comparison_self: 0,
                        span: 0,
                        children: Vec::new(),
                    });
                    if parent != u32::MAX {
                        nodes[parent as usize].children.push(id);
                    }
                    child.insert((parent, frame), id);
                    id
                };
                let node = &mut nodes[id as usize];
                if is_after {
                    node.comparison = node.comparison.saturating_add(weight);
                    if index == self_at {
                        node.comparison_self = node.comparison_self.saturating_add(weight);
                    }
                } else {
                    node.baseline = node.baseline.saturating_add(weight);
                    if index == self_at {
                        node.baseline_self = node.baseline_self.saturating_add(weight);
                    }
                }
                parent = id;
            }
        };

    diff.before.for_each_sample(|stack, weight| {
        ingest(
            stack,
            &diff.before_frame_map,
            weight,
            diff.before_scale,
            false,
        );
    });
    diff.after.for_each_sample(|stack, weight| {
        ingest(stack, &diff.after_frame_map, weight, diff.after_scale, true);
    });

    let mut roots: Vec<u32> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.parent.is_none())
        .map(|(id, _)| id as u32)
        .collect();

    // Compute bottom-up `span` without recursion.
    let mut postorder = Vec::with_capacity(nodes.len());
    for &root in &roots {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            postorder.push(id);
            for &kid in &nodes[id as usize].children {
                stack.push(kid);
            }
        }
    }
    for &id in postorder.iter().rev() {
        let children_span = nodes[id as usize].children.iter().fold(0_u64, |acc, &kid| {
            acc.saturating_add(nodes[kid as usize].span)
        });
        let node = &mut nodes[id as usize];
        let self_env = node.baseline_self.max(node.comparison_self);
        node.span = self_env
            .saturating_add(children_span)
            .max(node.baseline.max(node.comparison));
    }

    sort_diff_ids(&mut roots, &nodes);
    let mut order = Vec::with_capacity(nodes.len());
    for root in &roots {
        sort_diff_children(*root, &mut nodes);
        push_diff_preorder(*root, &nodes, &mut order);
    }
    let span_total = roots
        .iter()
        .fold(0_u64, |acc, &r| acc.saturating_add(nodes[r as usize].span));

    let mut remap = HashMap::with_capacity(order.len());
    for (new_id, &old_id) in order.iter().enumerate() {
        remap.insert(old_id, new_id as u32);
    }
    let tree = order
        .into_iter()
        .map(|old| {
            let n = &nodes[old as usize];
            DiffTreeNode {
                id: remap[&old],
                parent: n.parent.map(|p| remap[&p]),
                frame: n.frame,
                depth: n.depth,
                baseline: n.baseline,
                comparison: n.comparison,
                baseline_self: n.baseline_self,
                comparison_self: n.comparison_self,
                span: n.span,
            }
        })
        .collect();

    let mut fn_rows: Vec<DiffFunctionRow> = functions
        .into_iter()
        .map(
            |(frame, (baseline, comparison, baseline_self, comparison_self))| DiffFunctionRow {
                frame,
                baseline,
                comparison,
                baseline_self,
                comparison_self,
            },
        )
        .collect();
    fn_rows.sort_by(|a, b| {
        b.delta_self()
            .abs()
            .cmp(&a.delta_self().abs())
            .then_with(|| b.delta_total().abs().cmp(&a.delta_total().abs()))
            .then_with(|| b.comparison.cmp(&a.comparison))
            .then_with(|| a.frame.cmp(&b.frame))
    });

    DiffAggregate {
        baseline_total,
        comparison_total,
        baseline_samples,
        comparison_samples,
        span_total,
        nodes: tree,
        functions: fn_rows,
    }
}

fn node_abs_delta(node: &BuildDiffNode) -> u128 {
    (i128::from(node.comparison) - i128::from(node.baseline)).unsigned_abs()
}

fn sort_diff_ids(ids: &mut [u32], nodes: &[BuildDiffNode]) {
    ids.sort_by(|left, right| {
        let a = &nodes[*left as usize];
        let b = &nodes[*right as usize];
        b.span
            .cmp(&a.span)
            .then_with(|| node_abs_delta(b).cmp(&node_abs_delta(a)))
            .then_with(|| a.frame.cmp(&b.frame))
            .then_with(|| left.cmp(right))
    });
}

fn sort_diff_children(root: u32, nodes: &mut [BuildDiffNode]) {
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        let mut children = std::mem::take(&mut nodes[id as usize].children);
        sort_diff_ids(&mut children, nodes);
        nodes[id as usize].children = children;
        pending.extend(nodes[id as usize].children.iter().copied());
    }
}

fn push_diff_preorder(root: u32, nodes: &[BuildDiffNode], out: &mut Vec<u32>) {
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        out.push(id);
        for child in nodes[id as usize].children.iter().rev() {
            pending.push(*child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_diff_computes_regressions_improvements_and_union_spans() {
        let before = b"main;old_path 60\nmain;parse 20\n";
        let after = b"main;parse 90\nmain;new_hot 30\n";
        let diff = load_diff(before, after).expect("load_diff");
        let agg = diff_aggregate(&diff, &DiffQuery::default());
        assert_eq!(agg.baseline_total, 80);
        assert_eq!(agg.comparison_total, 120);
        assert_eq!(agg.delta_total(), 40);
        // Root `main` has `old_path` (span 60), `parse` (span 90), `new_hot` (span 30) -> span 180.
        assert_eq!(agg.span_total, 180);
        assert_eq!(agg.nodes[0].span, 180);

        let by_name: HashMap<&str, &DiffFunctionRow> = agg
            .functions
            .iter()
            .map(|row| (diff.frames[row.frame as usize].name.as_str(), row))
            .collect();
        assert_eq!(by_name["parse"].delta_self(), 70);
        assert_eq!(by_name["old_path"].delta_self(), -60);
        assert_eq!(by_name["new_hot"].delta_self(), 30);
        assert_eq!(by_name["main"].delta_total(), 40);
        assert_eq!(by_name["main"].delta_self(), 0);
    }

    #[test]
    fn incompatible_units_are_rejected_with_clear_error() {
        let counts = b"main;work 10\n";
        let speedscope_bytes = br#"{
          "$schema": "https://www.speedscope.app/file-format-schema.json",
          "shared": { "frames": [{ "name": "main" }, { "name": "alloc" }] },
          "profiles": [{
            "type": "sampled",
            "name": "heap",
            "unit": "bytes",
            "startValue": 0,
            "endValue": 1024,
            "samples": [[0, 1]],
            "weights": [1024]
          }]
        }"#;
        let err = load_diff(counts, speedscope_bytes).expect_err("bytes vs count must fail");
        let msg = err.to_string();
        assert!(msg.contains("incompatible weight units"), "{msg}");
    }

    #[test]
    fn line_shifts_merge_same_function_and_keep_after_line_number() {
        let before = br#"{
          "shared": { "frames": [{ "name": "[python] run", "file": "srv.py", "line": 10 }] },
          "profiles": [{ "type": "sampled", "name": "main", "unit": "milliseconds", "startValue": 0, "endValue": 50, "samples": [[0]], "weights": [50] }]
        }"#;
        let after = br#"{
          "shared": { "frames": [{ "name": "[python] run", "file": "srv.py", "line": 18 }] },
          "profiles": [{ "type": "sampled", "name": "main", "unit": "milliseconds", "startValue": 0, "endValue": 80, "samples": [[0]], "weights": [80] }]
        }"#;
        let diff = load_diff(before, after).expect("speedscope diff");
        assert_eq!(diff.frames.len(), 1);
        assert_eq!(diff.frames[0].line, Some(18));
        let agg = diff_aggregate(&diff, &DiffQuery::default());
        assert_eq!(agg.functions.len(), 1);
        assert_eq!(agg.functions[0].delta_total(), 30_000_000);
    }
}
