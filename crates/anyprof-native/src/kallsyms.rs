//! `/proc/kallsyms` text symbols.
//!
//! A zero address is hidden by `kernel.kptr_restrict` and is not a symbol.
//! Lookup uses the same one-byte return-address adjustment as user frames;
//! the caller passes the address it wants found.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]

/// One kernel text symbol: `(address, name)`, sorted by address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kallsym {
    /// Symbol address. Never zero.
    pub addr: u64,
    /// Name as printed by kallsyms. Kernel names are not Itanium-mangled.
    pub name: String,
}

/// Parse `kallsyms` lines. Only text symbols (`T`, `t`, `W`, `w`) are kept.
pub(crate) fn parse_kallsyms(text: &str) -> Vec<Kallsym> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(addr) = parts.next() else {
            continue;
        };
        let Some(kind) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        let Ok(addr) = u64::from_str_radix(addr, 16) else {
            continue;
        };
        if addr == 0 || name.is_empty() {
            continue;
        }
        let kind = kind.chars().next().unwrap_or(' ');
        if matches!(kind, 'T' | 't' | 'W' | 'w') {
            out.push(Kallsym {
                addr,
                name: name.to_string(),
            });
        }
    }
    out.sort_by_key(|sym| sym.addr);
    out.dedup_by(|right, left| right.addr == left.addr && right.name == left.name);
    out
}

/// Function containing `addr`, when a text symbol is at or below it.
///
/// The next symbol, or 1 MiB when this is the last one, ends the function.
/// A gap larger than that is not the same function.
pub(crate) fn name_at(symbols: &[Kallsym], addr: u64) -> Option<&str> {
    if addr == 0 || symbols.is_empty() {
        return None;
    }
    let idx = symbols.partition_point(|sym| sym.addr <= addr);
    let sym = symbols.get(idx.checked_sub(1)?)?;
    let next = symbols
        .get(idx)
        .map(|sym| sym.addr)
        .unwrap_or(sym.addr.saturating_add(1024 * 1024));
    if addr >= next || addr.saturating_sub(sym.addr) > 1024 * 1024 {
        return None;
    }
    Some(sym.name.as_str())
}

/// Whether any parsed symbol has a real address.
///
/// An empty result after a non-empty file means the kernel hid every address.
pub(crate) fn visible(symbols: &[Kallsym]) -> bool {
    !symbols.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{name_at, parse_kallsyms};

    #[test]
    fn text_symbols_are_kept_and_hidden_addresses_are_not() {
        let text = "\
ffffffff81000000 T startup_64
ffffffff81000100 t do_work
0000000000000000 t hidden
ffffffff81000200 D not_text
ffffffff81000180 W alias
";
        let symbols = parse_kallsyms(text);
        assert_eq!(symbols.len(), 3);
        assert_eq!(name_at(&symbols, 0xffff_ffff_8100_0000), Some("startup_64"));
        assert_eq!(name_at(&symbols, 0xffff_ffff_8100_0150), Some("do_work"));
        // `alias` starts at 0x180, so the byte before it still belongs to do_work.
        assert_eq!(name_at(&symbols, 0xffff_ffff_8100_017f), Some("do_work"));
        assert_eq!(name_at(&symbols, 0xffff_ffff_8100_0180), Some("alias"));
        assert_eq!(name_at(&symbols, 0), None);
        assert_eq!(name_at(&symbols, 0xffff_ffff_8000_0000), None);
    }
}
