use std::collections::HashMap;

use anyprof_core::{Frame, Profile, Stack};

use crate::error::Error;
use crate::symbol::{format_symbol, parse_symbol};

/// Fold `profile` into Brendan Gregg's collapsed-stack format.
///
/// Each line is `root;...;leaf count`, root first. Stacks that fold to the same
/// line are merged. `;` and newlines inside a symbol are replaced with `?` so
/// the line stays unambiguous.
pub(crate) fn export_string(profile: &Profile) -> String {
    let mut order = Vec::new();
    let mut totals: HashMap<Vec<String>, u64> = HashMap::new();
    for stack in &profile.stacks {
        if stack.weight == 0 {
            continue;
        }
        let key: Vec<String> = stack.frames.iter().rev().map(collapsed_token).collect();
        if let Some(total) = totals.get_mut(&key) {
            *total = total.saturating_add(stack.weight);
        } else {
            totals.insert(key.clone(), stack.weight);
            order.push(key);
        }
    }

    let mut out = String::new();
    for key in order {
        let weight = totals[&key];
        out.push_str(&key.join(";"));
        out.push(' ');
        out.push_str(&weight.to_string());
        out.push('\n');
    }
    out
}

pub(crate) fn import(bytes: &[u8]) -> Result<Profile, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| Error::invalid("collapsed", format!("not utf-8: {err}")))?;
    let mut stacks = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let stack = parse_line(line)
            .map_err(|reason| Error::invalid("collapsed", format!("line {}: {reason}", idx + 1)))?;
        stacks.push(stack);
    }
    Ok(Profile {
        stacks,
        ..Profile::default()
    })
}

fn collapsed_token(frame: &Frame) -> String {
    sanitize(&format_symbol(frame))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            ';' | '\n' | '\r' => '?',
            other => other,
        })
        .collect()
}

fn parse_line(line: &str) -> Result<Stack, String> {
    let (stack, count) = line
        .rsplit_once(' ')
        .ok_or_else(|| "missing sample count".to_string())?;
    if stack.is_empty() {
        return Err("empty stack".to_string());
    }
    let weight = count
        .parse::<u64>()
        .map_err(|_| format!("invalid sample count `{count}`"))?;
    let frames = stack
        .split(';')
        .rev()
        .map(|token| {
            let (runtime, name) = parse_symbol(token);
            Frame::new(runtime, name)
        })
        .collect();
    Ok(Stack { frames, weight })
}

#[cfg(test)]
mod tests {
    use super::export_string;
    use crate::{export, import, Format};
    use anyprof_core::{Frame, Profile, Runtime, Stack};

    #[test]
    fn writes_root_first_and_merges_identical_folds() {
        let text = export_string(&crate::fixture());
        assert_eq!(
            text,
            "\
main;[node] work 3
[kernel] schedule;[go] runtime.main 4
"
        );
    }

    #[test]
    fn roundtrip_keeps_runtime_name_and_weight() {
        let bytes = export(&crate::fixture(), Format::Collapsed).unwrap();
        let profile = import(&bytes, Format::Collapsed).unwrap();
        assert_eq!(profile.stacks.len(), 2);
        assert_eq!(profile.total_weight(), 7);
        let leaf = &profile.stacks[0].frames[0];
        assert_eq!(leaf.runtime, Runtime::Node);
        assert_eq!(leaf.name, "work");
        assert_eq!(leaf.file, None);
        assert_eq!(profile.stacks[0].weight, 3);
        let go_leaf = &profile.stacks[1].frames[0];
        assert_eq!(go_leaf.runtime, Runtime::Go);
        assert_eq!(go_leaf.name, "runtime.main");
        assert_eq!(profile.stacks[1].frames[1].runtime, Runtime::Kernel);
    }

    #[test]
    fn escapes_separators_inside_symbols() {
        let profile = Profile {
            stacks: vec![Stack {
                frames: vec![Frame::new(Runtime::Native, "a;b\nc")],
                weight: 1,
            }],
            ..Profile::default()
        };
        assert_eq!(export_string(&profile), "a?b?c 1\n");
    }

    #[test]
    fn skips_comments_blank_lines_and_rejects_a_bad_count() {
        let profile = import(b"# comment\n\nmain;leaf 8\n", Format::Collapsed).unwrap();
        assert_eq!(profile.stacks.len(), 1);
        assert_eq!(profile.stacks[0].weight, 8);
        let err = import(b"main;leaf no\n", Format::Collapsed).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("line 1"), "{message}");
    }

    #[test]
    fn empty_profile_is_an_empty_file() {
        let bytes = export(&Profile::default(), Format::Collapsed).unwrap();
        assert!(bytes.is_empty());
        let profile = import(&bytes, Format::Collapsed).unwrap();
        assert!(profile.stacks.is_empty());
    }
}
