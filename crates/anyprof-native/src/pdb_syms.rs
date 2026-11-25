//! Windows PDB fallback for addresses dbghelp does not name.
//!
//! A C/C++ image is named by `SymFromAddr`. A rustc image is not always:
//! the same call returns no name for every instruction in the module, so the
//! miss is not a short name buffer. The PE CodeView record names the PDB
//! rustc wrote (`/PDBALTPATH` often stores only the file name) and carries
//! the GUID and age of that file. This module reads a candidate only when
//! those match, then maps an RVA to a procedure.
//!
//! The lookup itself has no Windows dependency, so the range tests run on
//! every host. Opening a PDB is Windows x64 only, next to the sampler.

#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

#[cfg(all(windows, target_arch = "x86_64"))]
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyprof_core::Runtime;

struct Func {
    rva: u32,
    len: u32,
    name: String,
    runtime: Runtime,
    from_cpp: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineSpan {
    rva: u32,
    end: u32,
    file: u32,
    line: u32,
}

/// Functions and source line spans from one PDB, sorted by RVA.
pub(crate) struct ImageSymbols {
    funcs: Vec<Func>,
    files: Vec<String>,
    lines: Vec<LineSpan>,
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

    pub(crate) fn line_at(&self, rva: u64) -> Option<(&str, u32)> {
        let rva = u32::try_from(rva).ok()?;
        let idx = self.lines.partition_point(|span| span.rva <= rva);
        let span = self.lines.get(idx.checked_sub(1)?)?;
        if rva >= span.end || span.line == 0 {
            return None;
        }
        let file = self.files.get(span.file as usize)?;
        Some((file.as_str(), span.line))
    }
}

/// PDB paths to try: the CodeView name, then the image with a `.pdb` suffix.
///
/// A relative CodeView path is joined onto the executable's directory only
/// when none of its components is `..`. `dir.join` would otherwise follow
/// `..\..\other.pdb` out of that directory. An absolute path is kept as
/// written; the GUID check still has to accept the file.
pub(crate) fn candidate_pdb_paths(image: &Path, embedded: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(embedded) = embedded.map(str::trim).filter(|text| !text.is_empty())
        && let Some(path) = embedded_pdb_path(image, embedded)
    {
        paths.push(path);
    }
    paths.push(image.with_extension("pdb"));
    paths.dedup();
    paths
}

fn embedded_pdb_path(image: &Path, embedded: &str) -> Option<PathBuf> {
    if is_absolute_pdb_path(embedded) {
        return Some(PathBuf::from(embedded));
    }
    let dir = image.parent().filter(|dir| !dir.as_os_str().is_empty())?;
    let mut path = dir.to_path_buf();
    for part in relative_pdb_parts(embedded)? {
        path.push(part);
    }
    Some(path)
}

/// Drive, UNC, and rooted paths. `Path::is_absolute` is wrong for `C:\...`
/// when this check runs in a Linux test.
fn is_absolute_pdb_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes
        .first()
        .is_some_and(|byte| *byte == b'/' || *byte == b'\\')
    {
        return true;
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Components of a relative PDB path that stay inside the image directory.
fn relative_pdb_parts(text: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    for part in text.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') || part.contains('\0') {
            return None;
        }
        parts.push(part);
    }
    if parts.is_empty() { None } else { Some(parts) }
}

/// CodeView `RSDS` identity. `guid` is the 16 raw file bytes, not UUID order.
struct CodeView {
    guid: [u8; 16],
    age: u32,
    path: String,
}

/// GUID bytes in the order `uuid::Uuid::as_bytes` returns.
///
/// The RSDS record stores the first three GUID fields little-endian. `pdb`
/// rebuilds them with `Uuid::from_fields`, whose `as_bytes` is big-endian
/// for those fields.
fn rsds_uuid_bytes(raw: &[u8; 16]) -> [u8; 16] {
    let mut out = [0_u8; 16];
    out[0..4].copy_from_slice(&u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]).to_be_bytes());
    out[4..6].copy_from_slice(&u16::from_le_bytes([raw[4], raw[5]]).to_be_bytes());
    out[6..8].copy_from_slice(&u16::from_le_bytes([raw[6], raw[7]]).to_be_bytes());
    out[8..16].copy_from_slice(&raw[8..16]);
    out
}

/// A PDB matches the image when the GUID is the same, the PDB-info age is at
/// least the image age, and the DBI age equals the image age when present.
///
/// `pdb`'s own docs: info-stream age can be bumped by tools after the link,
/// and a DBI age of 0 means the stream has no age to compare.
fn signature_matches(
    image_guid: &[u8; 16],
    image_age: u32,
    pdb_guid: &[u8; 16],
    info_age: u32,
    dbi_age: Option<u32>,
) -> bool {
    if pdb_guid != &rsds_uuid_bytes(image_guid) || info_age < image_age {
        return false;
    }
    dbi_age.is_none_or(|age| age == image_age)
}

/// Read the CodeView record by seeking to the debug directory.
///
/// The directory is not required to sit in the first 1 MiB. Only the DOS
/// stub, the PE headers, the section table, the debug directory, and the
/// `RSDS` record itself are read.
fn read_codeview<R: std::io::Read + std::io::Seek>(file: &mut R) -> Option<CodeView> {
    use std::io::SeekFrom;
    let mut dos = [0_u8; 0x40];
    file.read_exact(&mut dos).ok()?;
    if dos[0..2] != *b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(dos[0x3C..0x40].try_into().ok()?) as u64;
    if e_lfanew > 1024 * 1024 {
        return None;
    }
    file.seek(SeekFrom::Start(e_lfanew)).ok()?;
    let mut coff = [0_u8; 24];
    file.read_exact(&mut coff).ok()?;
    if coff[0..4] != *b"PE\0\0" {
        return None;
    }
    let sections = u16::from_le_bytes(coff[6..8].try_into().ok()?) as usize;
    let opt_size = u16::from_le_bytes(coff[20..22].try_into().ok()?) as usize;
    if sections > 512 || opt_size > 1024 * 1024 {
        return None;
    }
    let mut opt = vec![0_u8; opt_size];
    file.read_exact(&mut opt).ok()?;
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
    let mut section_bytes = vec![0_u8; sections * 40];
    file.read_exact(&mut section_bytes).ok()?;
    let debug_off = rva_to_file_offset(&section_bytes, debug_rva)?;
    file.seek(SeekFrom::Start(debug_off)).ok()?;
    let mut dir = vec![0_u8; debug_size];
    file.read_exact(&mut dir).ok()?;
    let (ptr, size) = codeview_file_range(&dir)?;
    let mut data = vec![0_u8; size];
    file.seek(SeekFrom::Start(ptr)).ok()?;
    file.read_exact(&mut data).ok()?;
    parse_rsds(&data)
}

fn rva_to_file_offset(sections: &[u8], rva: u32) -> Option<u64> {
    let (sections, _rest) = sections.as_chunks::<40>();
    for sec in sections {
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

fn codeview_file_range(dir: &[u8]) -> Option<(u64, usize)> {
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

fn parse_rsds(data: &[u8]) -> Option<CodeView> {
    if data.len() < 24 || data[0..4] != *b"RSDS" {
        return None;
    }
    let mut guid = [0_u8; 16];
    guid.copy_from_slice(&data[4..20]);
    let age = u32::from_le_bytes(data[20..24].try_into().ok()?);
    let path = &data[24..];
    let end = path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path.len());
    let text = std::str::from_utf8(&path[..end]).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(CodeView {
        guid,
        age,
        path: text.to_string(),
    })
}

fn index(mut funcs: Vec<Func>) -> Vec<Func> {
    funcs.sort_by(|left, right| left.rva.cmp(&right.rva).then(rank(right).cmp(&rank(left))));
    funcs.dedup_by(|later, earlier| later.rva == earlier.rva);
    funcs
}

fn index_lines(mut spans: Vec<LineSpan>) -> Vec<LineSpan> {
    spans.sort_by(|left, right| left.rva.cmp(&right.rva).then(left.end.cmp(&right.end)));
    let mut merged: Vec<LineSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        if span.rva >= span.end || span.line == 0 {
            continue;
        }
        if let Some(prev) = merged.last_mut() {
            if span.rva < prev.end {
                prev.end = span.rva;
            }
            if prev.rva >= prev.end {
                *prev = span;
                continue;
            }
            if prev.end == span.rva && prev.file == span.file && prev.line == span.line {
                prev.end = span.end;
                continue;
            }
        }
        merged.push(span);
    }
    merged
}

/// Longer record first, then a Rust name over a native one.
///
/// A zero-length public used to outrank a real procedure when the public was
/// classified as Rust, and the kept record then covered at most 64 KiB. The
/// length is the range, so it wins. Kind only breaks a tie.
fn rank(func: &Func) -> (u32, u8) {
    let kind = if func.runtime == Runtime::Rust {
        2
    } else if !func.from_cpp && func.name.contains("::") {
        1
    } else {
        0
    };
    (func.len, kind)
}

/// A PDB larger than this is not parsed. The address stays unnamed.
const MAX_PDB_BYTES: u64 = 1024 * 1024 * 1024;

/// Read procedure and public symbols from the PDB next to `image`.
///
/// Failure is an empty table. The caller keeps the unnamed address.
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn load(image: &Path) -> ImageSymbols {
    let codeview = std::fs::File::open(image)
        .ok()
        .and_then(|mut file| read_codeview(&mut file));
    let embedded = codeview.as_ref().map(|cv| cv.path.as_str());
    for path in candidate_pdb_paths(image, embedded) {
        if let Some(symbols) = read_pdb(&path, codeview.as_ref()) {
            tracing::debug!(
                image = %image.display(),
                pdb = %path.display(),
                functions = symbols.funcs.len(),
                lines = symbols.lines.len(),
                "loaded PDB symbols"
            );
            return symbols;
        }
    }
    tracing::debug!(image = %image.display(), "no PDB symbols");
    ImageSymbols {
        funcs: Vec::new(),
        files: Vec::new(),
        lines: Vec::new(),
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn read_pdb(path: &Path, codeview: Option<&CodeView>) -> Option<ImageSymbols> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_pdb_inner(path, codeview)
    }))
    .unwrap_or_else(|_| {
        tracing::debug!(pdb = %path.display(), "PDB parser panicked; skipping PDB");
        None
    })
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn read_pdb_inner(path: &Path, codeview: Option<&CodeView>) -> Option<ImageSymbols> {
    use std::fs::File;

    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 || len > MAX_PDB_BYTES {
        tracing::debug!(pdb = %path.display(), len, "PDB was not parsed");
        return None;
    }
    let mut pdb = pdb::PDB::open(file).ok()?;
    if let Some(codeview) = codeview
        && !pdb_matches(&mut pdb, codeview)
    {
        tracing::debug!(pdb = %path.display(), "PDB GUID or age does not match the image");
        return None;
    }
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
    let (files, lines) = collect_lines(&mut pdb, &map);
    Some(ImageSymbols {
        funcs: index(funcs),
        files,
        lines,
    })
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn collect_lines(
    pdb: &mut pdb::PDB<'_, std::fs::File>,
    map: &pdb::AddressMap<'_>,
) -> (Vec<String>, Vec<LineSpan>) {
    use pdb::FallibleIterator;

    let Ok(strings) = pdb.string_table() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(dbi) = pdb.debug_information() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(mut modules) = dbi.modules() else {
        return (Vec::new(), Vec::new());
    };
    let mut files: Vec<String> = Vec::new();
    let mut file_ids: HashMap<String, u32> = HashMap::new();
    let mut spans: Vec<LineSpan> = Vec::new();

    while let Ok(Some(module)) = modules.next() {
        // `pdb 0.8` can trip internal bounds assertions on C13 line subsections
        // emitted by newer LLVM/MSVC linkers for `-O2` Rust code; isolate each
        // module so remaining modules and symbol tables still resolve.
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(Some(info)) = pdb.module_info(&module) else {
                return false;
            };
            let Ok(program) = info.line_program() else {
                return false;
            };
            let mut module_files: HashMap<u32, Option<u32>> = HashMap::new();
            let mut iter = program.lines();
            while let Ok(Some(line)) = iter.next() {
                if line.line_start == 0 || line.line_start >= 0x00ef_feff {
                    continue;
                }
                let Some(rva) = line.offset.to_rva(map) else {
                    continue;
                };
                let len = line.length.unwrap_or(64).clamp(1, 64 * 1024);
                let end = rva.0.saturating_add(len);
                let file_id = *module_files.entry(line.file_index.0).or_insert_with(|| {
                    let info = program.get_file_info(line.file_index).ok()?;
                    let name = info.name.to_string_lossy(&strings).ok()?;
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    if let Some(&existing) = file_ids.get(trimmed) {
                        return Some(existing);
                    }
                    let id = u32::try_from(files.len()).ok()?;
                    let owned = trimmed.to_string();
                    files.push(owned.clone());
                    file_ids.insert(owned, id);
                    Some(id)
                });
                if let Some(file) = file_id {
                    spans.push(LineSpan {
                        rva: rva.0,
                        end,
                        file,
                        line: line.line_start,
                    });
                    if spans.len() >= 4_000_000 {
                        return true;
                    }
                }
            }
            false
        }));
        match step {
            Ok(true) => return (files, index_lines(spans)),
            Ok(false) => {}
            Err(_) => {
                tracing::debug!("skipping PDB module line program after parser panic");
            }
        }
    }
    (files, index_lines(spans))
}

/// GUID plus age. Info-stream age may be higher than the image. DBI age, when
/// the PDB has one, must be equal. A mismatch skips this file.
#[cfg(all(windows, target_arch = "x86_64"))]
fn pdb_matches(pdb: &mut pdb::PDB<'_, std::fs::File>, codeview: &CodeView) -> bool {
    let Some((guid, info_age)) = (|| {
        let info = pdb.pdb_information().ok()?;
        Some((*info.guid.as_bytes(), info.age))
    })() else {
        return false;
    };
    let dbi_age = match pdb.debug_information() {
        Ok(dbi) => dbi.age(),
        Err(_) => return false,
    };
    signature_matches(&codeview.guid, codeview.age, &guid, info_age, dbi_age)
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
            let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Ok(Some(info)) = pdb.module_info(&module) else {
                    return false;
                };
                let Ok(mut symbols) = info.symbols() else {
                    return false;
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
                        return true;
                    }
                }
                false
            }));
            match step {
                Ok(true) => return raw,
                Ok(false) => {}
                Err(_) => {
                    tracing::debug!("skipping PDB module symbols after parser panic");
                }
            }
        }
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
    }));
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
    use super::{
        Func, ImageSymbols, LineSpan, candidate_pdb_paths, index, index_lines, read_codeview,
        rsds_uuid_bytes, signature_matches,
    };
    use anyprof_core::Runtime;
    use std::path::{Path, PathBuf};

    fn func(rva: u32, len: u32, name: &str, runtime: Runtime) -> Func {
        Func {
            rva,
            len,
            name: name.to_string(),
            runtime,
            from_cpp: false,
        }
    }

    fn symbols_from_funcs(funcs: Vec<Func>) -> ImageSymbols {
        ImageSymbols {
            funcs: index(funcs),
            files: Vec::new(),
            lines: Vec::new(),
        }
    }

    #[test]
    fn an_address_inside_a_procedure_uses_that_name() {
        let image = symbols_from_funcs(vec![
            func(0x1100, 0x20, "anyprof_rust_caller", Runtime::Native),
            func(
                0x1000,
                0x50,
                "anyprof_rust_fixture::anyprof_rust_spin",
                Runtime::Rust,
            ),
        ]);
        let (name, runtime, from_cpp) = image.name_at(0x1010).unwrap();
        assert_eq!(name, "anyprof_rust_fixture::anyprof_rust_spin");
        assert_eq!(runtime, Runtime::Rust);
        assert!(!from_cpp);
        assert!(image.name_at(0x1050).is_none());
        assert_eq!(image.name_at(0x1104).unwrap().0, "anyprof_rust_caller");
    }

    #[test]
    fn a_public_symbol_covers_up_to_the_next_one() {
        let image = symbols_from_funcs(vec![
            func(0x2000, 0, "second", Runtime::Rust),
            func(0x1000, 0, "first", Runtime::Rust),
        ]);
        assert_eq!(image.name_at(0x1FFF).unwrap().0, "first");
        assert_eq!(image.name_at(0x2000).unwrap().0, "second");
        assert!(image.name_at(0x2000 + 64 * 1024).is_none());
    }

    #[test]
    fn a_procedure_wins_over_a_public_at_the_same_rva() {
        let image = symbols_from_funcs(vec![
            func(0x1000, 0, "public_only", Runtime::Rust),
            func(0x1000, 0x40, "procedure", Runtime::Rust),
        ]);
        assert_eq!(image.name_at(0x1008).unwrap().0, "procedure");
    }

    #[test]
    fn a_long_procedure_beats_a_zero_length_rust_public() {
        let image = symbols_from_funcs(vec![
            func(0x1000, 0, "anyprof_rust_fixture::spin", Runtime::Rust),
            func(0x1000, 0x2000, "procedure", Runtime::Native),
        ]);
        assert_eq!(image.name_at(0x1100).unwrap().0, "procedure");
        assert_eq!(image.name_at(0x1000).unwrap().1, Runtime::Native);
        assert!(image.name_at(0x1000 + 0x2000).is_none());
    }

    #[test]
    fn line_at_clamps_overlapping_spans_and_resolves_rvas() {
        let image = ImageSymbols {
            funcs: Vec::new(),
            files: vec![r"C:\work\busy.rs".to_string()],
            lines: index_lines(vec![
                LineSpan {
                    rva: 0x1010,
                    end: 0x1030,
                    file: 0,
                    line: 24,
                },
                LineSpan {
                    rva: 0x1000,
                    end: 0x1040,
                    file: 0,
                    line: 20,
                },
            ]),
        };
        assert_eq!(image.line_at(0x0fff), None);
        assert_eq!(image.line_at(0x1008), Some((r"C:\work\busy.rs", 20)));
        assert_eq!(image.line_at(0x1010), Some((r"C:\work\busy.rs", 24)));
        assert_eq!(image.line_at(0x102f), Some((r"C:\work\busy.rs", 24)));
        assert_eq!(image.line_at(0x1030), None);
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
    fn a_relative_codeview_path_cannot_leave_the_image_directory() {
        let image = Path::new("out").join("rust-busy.exe");
        let sibling = Path::new("out").join("rust-busy.pdb");
        assert_eq!(
            candidate_pdb_paths(&image, Some(r"..\..\other.pdb")),
            vec![sibling.clone()]
        );
        assert_eq!(
            candidate_pdb_paths(&image, Some("sub/../other.pdb")),
            vec![sibling]
        );
        let nested = candidate_pdb_paths(&image, Some(r"symbols\fixture.pdb"));
        assert_eq!(
            nested[0],
            Path::new("out").join("symbols").join("fixture.pdb")
        );
        let absolute = candidate_pdb_paths(&image, Some(r"C:\sym\foo.pdb"));
        assert_eq!(absolute[0], PathBuf::from(r"C:\sym\foo.pdb"));
        assert_eq!(absolute[1], Path::new("out").join("rust-busy.pdb"));
    }

    #[test]
    fn codeview_rsds_path_is_read_from_a_pe() {
        let path = "anyprof_rust_fixture.pdb";
        let bytes = synthetic_pe32_plus(path, 0x200);
        let cv = read_codeview(&mut std::io::Cursor::new(bytes.as_slice())).unwrap();
        assert_eq!(cv.path, path);
        assert_eq!(cv.guid, RSDS_GUID);
        assert_eq!(cv.age, RSDS_AGE);
        assert_eq!(
            rsds_uuid_bytes(&cv.guid),
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
        assert!(read_codeview(&mut std::io::Cursor::new(&b"not a pe"[..])).is_none());
        assert!(read_codeview(&mut std::io::Cursor::new(&bytes[..64])).is_none());
    }

    #[test]
    fn codeview_past_the_first_mebibyte_is_still_read() {
        let path = "anyprof_rust_fixture.pdb";
        let bytes = synthetic_pe32_plus(path, 1024 * 1024 + 0x100);
        assert!(bytes.len() > 1024 * 1024);
        let cv = read_codeview(&mut std::io::Cursor::new(bytes.as_slice())).unwrap();
        assert_eq!(cv.path, path);
        assert_eq!(cv.guid, RSDS_GUID);
    }

    #[test]
    fn a_stale_pdb_signature_is_rejected() {
        let image = RSDS_GUID;
        let pdb = rsds_uuid_bytes(&image);
        assert!(signature_matches(
            &image,
            RSDS_AGE,
            &pdb,
            RSDS_AGE,
            Some(RSDS_AGE)
        ));
        assert!(signature_matches(
            &image,
            RSDS_AGE,
            &pdb,
            RSDS_AGE + 1,
            Some(RSDS_AGE)
        ));
        assert!(signature_matches(&image, RSDS_AGE, &pdb, RSDS_AGE, None));
        assert!(!signature_matches(
            &image,
            RSDS_AGE,
            &[0_u8; 16],
            RSDS_AGE,
            Some(RSDS_AGE)
        ));
        assert!(!signature_matches(
            &image,
            RSDS_AGE,
            &pdb,
            RSDS_AGE - 1,
            Some(RSDS_AGE)
        ));
        assert!(!signature_matches(
            &image,
            RSDS_AGE,
            &pdb,
            RSDS_AGE,
            Some(RSDS_AGE + 1)
        ));
    }

    /// Little-endian GUID fields as an `RSDS` record stores them.
    const RSDS_GUID: [u8; 16] = [
        0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const RSDS_AGE: u32 = 7;

    /// A PE32+ whose debug directory points at an `RSDS` path.
    ///
    /// `raw` is the file offset of that directory, so a value past 1 MiB
    /// covers a debug directory that a prefix scan would miss.
    fn synthetic_pe32_plus(pdb: &str, raw: usize) -> Vec<u8> {
        let pdb_bytes = {
            let mut bytes = pdb.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };
        let e_lfanew = 0x80usize;
        let opt_size = 0xF0usize;
        let section_off = e_lfanew + 24 + opt_size;
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
        bytes[cv + 4..cv + 20].copy_from_slice(&RSDS_GUID);
        bytes[cv + 20..cv + 24].copy_from_slice(&RSDS_AGE.to_le_bytes());
        bytes[cv + 24..cv + 24 + pdb_bytes.len()].copy_from_slice(&pdb_bytes);
        bytes
    }
}
