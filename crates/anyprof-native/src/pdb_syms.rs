//! Windows PDB fallback for addresses dbghelp does not name.
//!
//! A C/C++ image is named by `SymFromAddr`. A rustc image is not always:
//! the same call returns no name for every instruction in the module, so the
//! miss is not a short name buffer. The PE CodeView record names the PDB
//! rustc wrote (`/PDBALTPATH` often stores only the file name). This module
//! reads that file and maps an RVA to a procedure.
//!
//! The lookup itself has no Windows dependency, so the range tests run on
//! every host. Opening a PDB is Windows x64 only, next to the sampler.

#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

use std::path::{Path, PathBuf};

use anyprof_core::Runtime;

struct Func {
    rva: u32,
    len: u32,
    name: String,
    runtime: Runtime,
    from_cpp: bool,
}

/// Functions from one PDB, sorted by RVA.
pub(crate) struct ImageSymbols {
    funcs: Vec<Func>,
}

impl ImageSymbols {
    pub(crate) fn name_at(&self, rva: u64) -> Option<(&str, Runtime, bool)> {
        let rva = u32::try_from(rva).ok()?;
        let idx = self.funcs.partition_point(|func| func.rva <= rva);
        let func = self.funcs.get(idx.checked_sub(1)?)?;
        let next = self.funcs.get(idx).map(|func| func.rva);
        // A public symbol has no length. It covers up to the next symbol,
        // and not an unbounded tail after the last one.
        let natural = if func.len == 0 {
            func.rva.saturating_add(64 * 1024)
        } else {
            func.rva.saturating_add(func.len)
        };
        let end = next.map_or(natural, |next| natural.min(next));
        if rva >= end {
            return None;
        }
        Some((func.name.as_str(), func.runtime, func.from_cpp))
    }
}

/// PDB paths to try: the CodeView name, then the image with a `.pdb` suffix.
pub(crate) fn candidate_pdb_paths(image: &Path, embedded: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(embedded) = embedded.map(str::trim).filter(|text| !text.is_empty()) {
        let embedded = Path::new(embedded);
        if embedded.is_absolute() {
            paths.push(embedded.to_path_buf());
        } else if let Some(dir) = image.parent() {
            paths.push(dir.join(embedded));
        } else {
            paths.push(embedded.to_path_buf());
        }
    }
    paths.push(image.with_extension("pdb"));
    paths.dedup();
    paths
}

/// PDB path stored in a PE CodeView `RSDS` record, when `bytes` contains it.
pub(crate) fn codeview_pdb_path(bytes: &[u8]) -> Option<String> {
    let (offset, size) = locate_codeview(bytes)?;
    let start = usize::try_from(offset).ok()?;
    let data = bytes.get(start..start.checked_add(size)?)?;
    rsds_path(data)
}

fn locate_codeview(bytes: &[u8]) -> Option<(u64, usize)> {
    if bytes.len() < 0x40 || bytes[0..2] != *b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(bytes.get(0x3C..0x40)?.try_into().ok()?) as usize;
    let pe = bytes.get(e_lfanew..)?;
    if pe.len() < 24 || pe[0..4] != *b"PE\0\0" {
        return None;
    }
    let sections = u16::from_le_bytes(pe[6..8].try_into().ok()?) as usize;
    let opt_size = u16::from_le_bytes(pe[20..22].try_into().ok()?) as usize;
    let opt = pe.get(24..24 + opt_size)?;
    let magic = u16::from_le_bytes(opt.get(..2)?.try_into().ok()?);
    let (dir_at, dir_count) = match magic {
        0x20B if opt.len() >= 112 => (
            112,
            u32::from_le_bytes(opt[108..112].try_into().ok()?) as usize,
        ),
        0x10B if opt.len() >= 96 => (
            96,
            u32::from_le_bytes(opt[92..96].try_into().ok()?) as usize,
        ),
        _ => return None,
    };
    if dir_count <= 6 {
        return None;
    }
    let entry = opt.get(dir_at + 48..dir_at + 56)?;
    let debug_rva = u32::from_le_bytes(entry[0..4].try_into().ok()?);
    let debug_size = u32::from_le_bytes(entry[4..8].try_into().ok()?) as usize;
    if debug_rva == 0 || !(28..=1024 * 1024).contains(&debug_size) {
        return None;
    }
    let debug_off = rva_to_offset(bytes, e_lfanew, sections, opt_size, debug_rva)?;
    let debug_at = usize::try_from(debug_off).ok()?;
    let dir = bytes.get(debug_at..debug_at.checked_add(debug_size)?)?;
    let mut cursor = 0;
    while cursor + 28 <= dir.len() {
        let rec = &dir[cursor..cursor + 28];
        let kind = u32::from_le_bytes(rec[12..16].try_into().ok()?);
        let size = u32::from_le_bytes(rec[16..20].try_into().ok()?) as usize;
        let ptr = u32::from_le_bytes(rec[24..28].try_into().ok()?) as u64;
        // `IMAGE_DEBUG_TYPE_CODEVIEW`.
        if kind == 2 && (24..1024 * 1024).contains(&size) && ptr > 0 {
            return Some((ptr, size));
        }
        cursor += 28;
    }
    None
}

fn rva_to_offset(
    bytes: &[u8],
    e_lfanew: usize,
    sections: usize,
    opt_size: usize,
    rva: u32,
) -> Option<u64> {
    let start = e_lfanew + 24 + opt_size;
    for index in 0..sections {
        let off = start + index * 40;
        let sec = bytes.get(off..off + 40)?;
        let virt_size = u32::from_le_bytes(sec[8..12].try_into().ok()?);
        let virt = u32::from_le_bytes(sec[12..16].try_into().ok()?);
        let raw_size = u32::from_le_bytes(sec[16..20].try_into().ok()?);
        let raw_ptr = u32::from_le_bytes(sec[20..24].try_into().ok()?);
        let span = virt_size.max(raw_size);
        if rva >= virt && rva < virt.saturating_add(span) {
            return Some(u64::from(raw_ptr) + u64::from(rva - virt));
        }
    }
    None
}

fn rsds_path(data: &[u8]) -> Option<String> {
    if data.len() < 24 || data[0..4] != *b"RSDS" {
        return None;
    }
    let path = &data[24..];
    let end = path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path.len());
    let text = std::str::from_utf8(&path[..end]).ok()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn index(mut funcs: Vec<Func>) -> Vec<Func> {
    funcs.sort_by(|left, right| left.rva.cmp(&right.rva).then(rank(right).cmp(&rank(left))));
    funcs.dedup_by(|later, earlier| later.rva == earlier.rva);
    funcs
}

fn rank(func: &Func) -> (u8, u32) {
    let kind = if func.runtime == Runtime::Rust {
        2
    } else if !func.from_cpp && func.name.contains("::") {
        1
    } else {
        0
    };
    (kind, func.len)
}

/// Read procedure and public symbols from the PDB next to `image`.
///
/// Failure is an empty table. The caller keeps the unnamed address.
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn load(image: &Path) -> ImageSymbols {
    let embedded = std::fs::File::open(image)
        .ok()
        .and_then(|mut file| read_embedded_pdb(&mut file));
    for path in candidate_pdb_paths(image, embedded.as_deref()) {
        if let Some(funcs) = read_pdb(&path) {
            tracing::debug!(
                image = %image.display(),
                pdb = %path.display(),
                functions = funcs.len(),
                "loaded PDB symbols"
            );
            return ImageSymbols { funcs };
        }
    }
    tracing::debug!(image = %image.display(), "no PDB symbols");
    ImageSymbols { funcs: Vec::new() }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn read_embedded_pdb(file: &mut std::fs::File) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let len = file.metadata().ok()?.len();
    let header_len = usize::try_from(len.min(1024 * 1024)).ok()?;
    let mut buf = vec![0_u8; header_len];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    if let Some(path) = codeview_pdb_path(&buf) {
        return Some(path);
    }
    let (offset, size) = locate_codeview(&buf)?;
    let start = usize::try_from(offset).ok()?;
    if start.checked_add(size).is_some_and(|end| end <= buf.len()) {
        return None;
    }
    if size > 1024 * 1024 {
        return None;
    }
    let mut data = vec![0_u8; size];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut data).ok()?;
    rsds_path(&data)
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn read_pdb(path: &Path) -> Option<Vec<Func>> {
    use std::fs::File;

    let file = File::open(path).ok()?;
    let mut pdb = pdb::PDB::open(file).ok()?;
    let raw = collect_raw(&mut pdb);
    let map = pdb.address_map().ok()?;
    let mut funcs = Vec::with_capacity(raw.len());
    for sym in raw {
        let Some(rva) = sym.offset.to_rva(&map) else {
            continue;
        };
        funcs.push(Func {
            rva: rva.0,
            len: sym.len,
            name: sym.name,
            runtime: sym.runtime,
            from_cpp: sym.from_cpp,
        });
        if funcs.len() == 2_000_000 {
            break;
        }
    }
    Some(index(funcs))
}

#[cfg(all(windows, target_arch = "x86_64"))]
struct RawSym {
    offset: pdb::PdbInternalSectionOffset,
    len: u32,
    name: String,
    runtime: Runtime,
    from_cpp: bool,
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn collect_raw(pdb: &mut pdb::PDB<'_, std::fs::File>) -> Vec<RawSym> {
    use pdb::FallibleIterator;

    let mut raw = Vec::new();
    if let Ok(dbi) = pdb.debug_information()
        && let Ok(mut modules) = dbi.modules()
    {
        while let Ok(Some(module)) = modules.next() {
            let Ok(Some(info)) = pdb.module_info(&module) else {
                continue;
            };
            let Ok(mut symbols) = info.symbols() else {
                continue;
            };
            while let Ok(Some(symbol)) = symbols.next() {
                match symbol.parse() {
                    Ok(pdb::SymbolData::Procedure(proc)) => {
                        push_raw(&mut raw, proc.offset, proc.len, &proc.name.to_string());
                    }
                    Ok(pdb::SymbolData::Public(public)) if public.function || public.code => {
                        push_raw(&mut raw, public.offset, 0, &public.name.to_string());
                    }
                    _ => {}
                }
                if raw.len() == 2_000_000 {
                    return raw;
                }
            }
        }
    }
    if let Ok(globals) = pdb.global_symbols() {
        let mut symbols = globals.iter();
        while let Ok(Some(symbol)) = symbols.next() {
            if let Ok(pdb::SymbolData::Public(public)) = symbol.parse()
                && (public.function || public.code)
            {
                push_raw(&mut raw, public.offset, 0, &public.name.to_string());
            }
            if raw.len() == 2_000_000 {
                break;
            }
        }
    }
    raw
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn push_raw(raw: &mut Vec<RawSym>, offset: pdb::PdbInternalSectionOffset, len: u32, text: &str) {
    if text.is_empty() || text.len() > 64 * 1024 {
        return;
    }
    let presented = crate::symbol::present_elf_symbol(text);
    if presented.name.is_empty() {
        return;
    }
    raw.push(RawSym {
        offset,
        len,
        name: presented.name,
        runtime: presented.runtime,
        from_cpp: presented.from_cpp,
    });
}

#[cfg(test)]
mod tests {
    use super::{Func, ImageSymbols, candidate_pdb_paths, codeview_pdb_path, index};
    use anyprof_core::Runtime;
    use std::path::Path;

    fn func(rva: u32, len: u32, name: &str, runtime: Runtime) -> Func {
        Func {
            rva,
            len,
            name: name.to_string(),
            runtime,
            from_cpp: false,
        }
    }

    #[test]
    fn an_address_inside_a_procedure_uses_that_name() {
        let image = ImageSymbols {
            funcs: index(vec![
                func(0x1100, 0x20, "anyprof_rust_caller", Runtime::Native),
                func(
                    0x1000,
                    0x50,
                    "anyprof_rust_fixture::anyprof_rust_spin",
                    Runtime::Rust,
                ),
            ]),
        };
        let (name, runtime, from_cpp) = image.name_at(0x1010).unwrap();
        assert_eq!(name, "anyprof_rust_fixture::anyprof_rust_spin");
        assert_eq!(runtime, Runtime::Rust);
        assert!(!from_cpp);
        assert!(image.name_at(0x1050).is_none());
        assert_eq!(image.name_at(0x1104).unwrap().0, "anyprof_rust_caller");
    }

    #[test]
    fn a_public_symbol_covers_up_to_the_next_one() {
        let image = ImageSymbols {
            funcs: index(vec![
                func(0x2000, 0, "second", Runtime::Rust),
                func(0x1000, 0, "first", Runtime::Rust),
            ]),
        };
        assert_eq!(image.name_at(0x1FFF).unwrap().0, "first");
        assert_eq!(image.name_at(0x2000).unwrap().0, "second");
        assert!(image.name_at(0x2000 + 64 * 1024).is_none());
    }

    #[test]
    fn a_procedure_wins_over_a_public_at_the_same_rva() {
        let image = ImageSymbols {
            funcs: index(vec![
                func(0x1000, 0, "public_only", Runtime::Rust),
                func(0x1000, 0x40, "procedure", Runtime::Rust),
            ]),
        };
        assert_eq!(image.name_at(0x1008).unwrap().0, "procedure");
    }

    #[test]
    fn candidates_prefer_the_codeview_file_name() {
        let image = Path::new("out").join("rust-busy.exe");
        let paths = candidate_pdb_paths(&image, Some("anyprof_rust_fixture.pdb"));
        assert_eq!(paths[0], Path::new("out").join("anyprof_rust_fixture.pdb"));
        assert_eq!(paths[1], Path::new("out").join("rust-busy.pdb"));
        let only = candidate_pdb_paths(&image, None);
        assert_eq!(only, vec![Path::new("out").join("rust-busy.pdb")]);
    }

    #[test]
    fn codeview_rsds_path_is_read_from_a_pe() {
        let path = "anyprof_rust_fixture.pdb";
        let bytes = synthetic_pe32_plus(path);
        assert_eq!(codeview_pdb_path(&bytes).as_deref(), Some(path));
        assert_eq!(codeview_pdb_path(b"not a pe"), None);
        assert_eq!(codeview_pdb_path(&bytes[..64]), None);
    }

    /// A PE32+ whose debug directory points at an `RSDS` path.
    fn synthetic_pe32_plus(pdb: &str) -> Vec<u8> {
        let pdb_bytes = {
            let mut bytes = pdb.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };
        let e_lfanew = 0x80usize;
        let opt_size = 0xF0usize;
        let section_off = e_lfanew + 24 + opt_size;
        let raw = 0x200usize;
        let debug_rva = 0x1000u32;
        let cv_size = 24 + pdb_bytes.len();
        let mut bytes = vec![0_u8; raw + 28 + cv_size];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        bytes[0x3C..0x40].copy_from_slice(&(e_lfanew as u32).to_le_bytes());
        bytes[e_lfanew..e_lfanew + 4].copy_from_slice(b"PE\0\0");
        bytes[e_lfanew + 6..e_lfanew + 8].copy_from_slice(&1u16.to_le_bytes());
        bytes[e_lfanew + 20..e_lfanew + 22].copy_from_slice(&(opt_size as u16).to_le_bytes());
        let opt = e_lfanew + 24;
        bytes[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        bytes[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let debug = opt + 112 + 6 * 8;
        bytes[debug..debug + 4].copy_from_slice(&debug_rva.to_le_bytes());
        bytes[debug + 4..debug + 8].copy_from_slice(&28u32.to_le_bytes());
        let sec = section_off;
        bytes[sec + 8..sec + 12].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[sec + 12..sec + 16].copy_from_slice(&debug_rva.to_le_bytes());
        bytes[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[sec + 20..sec + 24].copy_from_slice(&(raw as u32).to_le_bytes());
        let dir = raw;
        bytes[dir + 12..dir + 16].copy_from_slice(&2u32.to_le_bytes());
        bytes[dir + 16..dir + 20].copy_from_slice(&(cv_size as u32).to_le_bytes());
        bytes[dir + 24..dir + 28].copy_from_slice(&((raw + 28) as u32).to_le_bytes());
        let cv = raw + 28;
        bytes[cv..cv + 4].copy_from_slice(b"RSDS");
        bytes[cv + 24..cv + 24 + pdb_bytes.len()].copy_from_slice(&pdb_bytes);
        bytes
    }
}
