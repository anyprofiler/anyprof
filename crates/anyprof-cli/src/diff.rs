use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, anyhow};
use anyprof_export::{
    DiffAggregate, DiffFunctionRow, DiffQuery, DiffTrace, WeightKind, diff_aggregate, load_diff,
};
use serde_json::json;

use crate::cli::DiffArgs;
use crate::view;

pub fn run(args: DiffArgs) -> anyhow::Result<()> {
    if !args.web
        && let Some(flag) = args.viewer.first_flag_set()
    {
        return Err(anyhow!("{flag} configures the viewer and requires --web"));
    }
    view::validate(&args.viewer)?;

    let before_path = std::path::absolute(&args.before).unwrap_or_else(|_| args.before.clone());
    let after_path = std::path::absolute(&args.after).unwrap_or_else(|_| args.after.clone());
    let before_bytes = read_profile(&before_path)?;
    let after_bytes = read_profile(&after_path)?;

    let diff = load_diff(&before_bytes, &after_bytes).map_err(|err| {
        anyhow!(
            "cannot compare {} and {}: {err}",
            before_path.display(),
            after_path.display()
        )
    })?;
    let agg = diff_aggregate(
        &diff,
        &DiffQuery {
            invert: false,
            hide_system: args.hide_system,
        },
    );

    let before_name = display_name(&before_path, "before");
    let after_name = display_name(&after_path, "after");
    let top_n = args.top.max(1);

    let summary = render_text_report(&before_name, &after_name, &diff, &agg, top_n);
    print!("{summary}");

    if let Some(out_raw) = &args.output {
        let out_path = std::path::absolute(out_raw).unwrap_or_else(|_| out_raw.clone());
        if let Some(parent) = out_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let ext = out_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = match ext.as_str() {
            "json" => render_json_report(&before_name, &after_name, &diff, &agg, top_n)?,
            "md" | "markdown" => {
                render_markdown_report(&before_name, &after_name, &diff, &agg, top_n).into_bytes()
            }
            _ => summary.into_bytes(),
        };
        std::fs::write(&out_path, bytes)
            .with_context(|| format!("writing {}", out_path.display()))?;
        eprintln!("Wrote diff report to {}", out_path.display());
    }

    if args.web {
        view::show_diff(
            &before_path,
            before_bytes,
            &after_path,
            after_bytes,
            &args.viewer,
        )?;
    }
    Ok(())
}

fn read_profile(path: &Path) -> anyhow::Result<Vec<u8>> {
    if path.is_dir() {
        return Err(anyhow!("{} is a directory, not a profile", path.display()));
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.is_empty() {
        return Err(anyhow!("{} is empty", path.display()));
    }
    Ok(bytes)
}

fn display_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback.to_string())
}

fn format_weight(value: u64, kind: WeightKind, unit: &str) -> String {
    match kind {
        WeightKind::SampledTime | WeightKind::CpuTime | WeightKind::Duration => {
            if value == 0 {
                "0".to_string()
            } else if value >= 1_000_000_000 {
                format!("{:.2} s", value as f64 / 1e9)
            } else if value >= 1_000_000 {
                format!("{:.2} ms", value as f64 / 1e6)
            } else if value >= 1_000 {
                format!("{:.1} us", value as f64 / 1e3)
            } else {
                format!("{value} ns")
            }
        }
        WeightKind::Bytes => {
            if value >= 1_000_000_000 {
                format!("{:.2} GB", value as f64 / 1e9)
            } else if value >= 1_000_000 {
                format!("{:.2} MB", value as f64 / 1e6)
            } else if value >= 1_000 {
                format!("{:.1} KB", value as f64 / 1e3)
            } else {
                format!("{value} B")
            }
        }
        WeightKind::Other if !unit.is_empty() => format!("{value} {unit}"),
        _ => format!("{value}"),
    }
}

fn format_signed(delta: i128, kind: WeightKind, unit: &str) -> String {
    if delta == 0 {
        return "0".to_string();
    }
    let sign = if delta > 0 { "+" } else { "-" };
    let abs = u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX);
    format!("{sign}{}", format_weight(abs, kind, unit))
}

fn format_signed_pct(delta: i128, baseline: u64) -> String {
    if delta == 0 {
        return "0.0%".to_string();
    }
    if baseline == 0 {
        return if delta > 0 {
            "new".to_string()
        } else {
            "-100.0%".to_string()
        };
    }
    let pct = (delta as f64 / baseline as f64) * 100.0;
    if pct > 0.0 {
        format!("+{pct:.1}%")
    } else {
        format!("{pct:.1}%")
    }
}

fn select_regressions_and_improvements(
    functions: &[DiffFunctionRow],
    top: usize,
) -> (Vec<&DiffFunctionRow>, Vec<&DiffFunctionRow>) {
    let mut regressions: Vec<&DiffFunctionRow> = functions
        .iter()
        .filter(|row| row.delta_self() > 0 || (row.delta_self() == 0 && row.delta_total() > 0))
        .collect();
    regressions.sort_by(|a, b| {
        b.delta_self()
            .cmp(&a.delta_self())
            .then_with(|| b.delta_total().cmp(&a.delta_total()))
            .then_with(|| a.frame.cmp(&b.frame))
    });
    regressions.truncate(top);

    let mut improvements: Vec<&DiffFunctionRow> = functions
        .iter()
        .filter(|row| row.delta_self() < 0 || (row.delta_self() == 0 && row.delta_total() < 0))
        .collect();
    improvements.sort_by(|a, b| {
        a.delta_self()
            .cmp(&b.delta_self())
            .then_with(|| a.delta_total().cmp(&b.delta_total()))
            .then_with(|| a.frame.cmp(&b.frame))
    });
    improvements.truncate(top);

    (regressions, improvements)
}

fn frame_label(diff: &DiffTrace, frame_id: u32) -> String {
    let Some(frame) = diff.frames.get(frame_id as usize) else {
        return "unknown".to_string();
    };
    if let Some(file) = frame.file.as_deref().filter(|f| !f.is_empty()) {
        let short = Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file);
        match frame.line {
            Some(line) if line > 0 => format!("{} ({short}:{line})", frame.name),
            _ => format!("{} ({short})", frame.name),
        }
    } else if let Some(module) = frame.module.as_deref().filter(|m| !m.is_empty()) {
        format!("{} ({module})", frame.name)
    } else {
        frame.name.clone()
    }
}

fn render_text_report(
    before_name: &str,
    after_name: &str,
    diff: &DiffTrace,
    agg: &DiffAggregate,
    top: usize,
) -> String {
    let kind = diff.weight_kind;
    let unit = &diff.unit;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Diff: {before_name} ({}) -> {after_name} ({})",
        diff.before.format, diff.after.format
    );
    let _ = writeln!(
        out,
        "Baseline: {} ({} samples)  |  After: {} ({} samples)  |  Net delta: {} ({})",
        format_weight(agg.baseline_total, kind, unit),
        agg.baseline_samples,
        format_weight(agg.comparison_total, kind, unit),
        agg.comparison_samples,
        format_signed(agg.delta_total(), kind, unit),
        format_signed_pct(agg.delta_total(), agg.baseline_total),
    );
    for note in &diff.notes {
        let _ = writeln!(out, "Note: {note}");
    }

    let (regressions, improvements) = select_regressions_and_improvements(&agg.functions, top);
    if !regressions.is_empty() {
        let _ = writeln!(out, "\nTop regressions (+delta):");
        for row in regressions {
            let _ = writeln!(
                out,
                "  {:>12} self  {:>12} total ({:>7})  {}",
                format_signed(row.delta_self(), kind, unit),
                format_signed(row.delta_total(), kind, unit),
                format_signed_pct(row.delta_total(), row.baseline),
                frame_label(diff, row.frame),
            );
        }
    }
    if !improvements.is_empty() {
        let _ = writeln!(out, "\nTop improvements (-delta):");
        for row in improvements {
            let _ = writeln!(
                out,
                "  {:>12} self  {:>12} total ({:>7})  {}",
                format_signed(row.delta_self(), kind, unit),
                format_signed(row.delta_total(), kind, unit),
                format_signed_pct(row.delta_total(), row.baseline),
                frame_label(diff, row.frame),
            );
        }
    }
    if agg
        .functions
        .iter()
        .all(|r| r.delta_total() == 0 && r.delta_self() == 0)
    {
        let _ = writeln!(
            out,
            "\nNo weight differences between baseline and comparison."
        );
    }
    out
}

fn render_markdown_report(
    before_name: &str,
    after_name: &str,
    diff: &DiffTrace,
    agg: &DiffAggregate,
    top: usize,
) -> String {
    let kind = diff.weight_kind;
    let unit = &diff.unit;
    let mut out = String::new();
    let _ = writeln!(out, "# Profile Diff: `{before_name}` -> `{after_name}`\n");
    let _ = writeln!(
        out,
        "- **Baseline**: {} ({} samples, `{}`)",
        format_weight(agg.baseline_total, kind, unit),
        agg.baseline_samples,
        diff.before.format,
    );
    let _ = writeln!(
        out,
        "- **After**: {} ({} samples, `{}`)",
        format_weight(agg.comparison_total, kind, unit),
        agg.comparison_samples,
        diff.after.format,
    );
    let _ = writeln!(
        out,
        "- **Net Delta**: **{}** ({})\n",
        format_signed(agg.delta_total(), kind, unit),
        format_signed_pct(agg.delta_total(), agg.baseline_total),
    );

    let (regressions, improvements) = select_regressions_and_improvements(&agg.functions, top);
    if !regressions.is_empty() {
        let _ = writeln!(out, "## Top Regressions\n");
        let _ = writeln!(out, "| Baseline | After | Δ Total | Δ Self | Symbol |");
        let _ = writeln!(out, "|---:|---:|---:|---:|:---|");
        for row in regressions {
            let _ = writeln!(
                out,
                "| {} | {} | {} ({}) | {} | `{}` |",
                format_weight(row.baseline, kind, unit),
                format_weight(row.comparison, kind, unit),
                format_signed(row.delta_total(), kind, unit),
                format_signed_pct(row.delta_total(), row.baseline),
                format_signed(row.delta_self(), kind, unit),
                frame_label(diff, row.frame).replace('`', "'"),
            );
        }
        let _ = writeln!(out);
    }
    if !improvements.is_empty() {
        let _ = writeln!(out, "## Top Improvements\n");
        let _ = writeln!(out, "| Baseline | After | Δ Total | Δ Self | Symbol |");
        let _ = writeln!(out, "|---:|---:|---:|---:|:---|");
        for row in improvements {
            let _ = writeln!(
                out,
                "| {} | {} | {} ({}) | {} | `{}` |",
                format_weight(row.baseline, kind, unit),
                format_weight(row.comparison, kind, unit),
                format_signed(row.delta_total(), kind, unit),
                format_signed_pct(row.delta_total(), row.baseline),
                format_signed(row.delta_self(), kind, unit),
                frame_label(diff, row.frame).replace('`', "'"),
            );
        }
    }
    out
}

fn render_json_report(
    before_name: &str,
    after_name: &str,
    diff: &DiffTrace,
    agg: &DiffAggregate,
    top: usize,
) -> anyhow::Result<Vec<u8>> {
    let clamp_delta = |d: i128| d.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    let (regressions, improvements) = select_regressions_and_improvements(&agg.functions, top);
    let encode_row = |row: &DiffFunctionRow| {
        let frame = &diff.frames[row.frame as usize];
        json!({
            "name": frame.name,
            "runtime": frame.runtime.as_str(),
            "file": frame.file,
            "line": frame.line,
            "module": frame.module,
            "baseline": row.baseline,
            "comparison": row.comparison,
            "baselineSelf": row.baseline_self,
            "comparisonSelf": row.comparison_self,
            "delta": clamp_delta(row.delta_total()),
            "deltaSelf": clamp_delta(row.delta_self()),
        })
    };
    let doc = json!({
        "before": {
            "name": before_name,
            "format": diff.before.format,
            "total": agg.baseline_total,
            "samples": agg.baseline_samples,
        },
        "after": {
            "name": after_name,
            "format": diff.after.format,
            "total": agg.comparison_total,
            "samples": agg.comparison_samples,
        },
        "weightKind": diff.weight_kind.as_str(),
        "unit": diff.unit,
        "deltaTotal": clamp_delta(agg.delta_total()),
        "notes": diff.notes,
        "regressions": regressions.into_iter().map(encode_row).collect::<Vec<_>>(),
        "improvements": improvements.into_iter().map(encode_row).collect::<Vec<_>>(),
    });
    let mut bytes = serde_json::to_vec_pretty(&doc)?;
    bytes.push(b'\n');
    Ok(bytes)
}
