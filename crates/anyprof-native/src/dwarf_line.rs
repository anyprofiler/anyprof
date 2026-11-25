//! DWARF `.debug_line` (v2–v5) address-to-source-line tables.
//!
//! Parses `.debug_line` (plus optional `.debug_line_str` and `.debug_str`
//! sections for DWARF v5 string offsets) into a sorted, bounded [`LineTable`]
//! without external crate dependencies. Malformed or oversized units are
//! skipped rather than failing symbol loading.

#![cfg_attr(windows, allow(dead_code))]

use std::collections::HashMap;

/// Maximum `.debug_line` section size we will read into memory (64 MiB).
pub(crate) const MAX_LINE_SECTION_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum `.debug_line_str` or `.debug_str` section size (64 MiB).
pub(crate) const MAX_STR_SECTION_BYTES: u64 = 64 * 1024 * 1024;

const MAX_DIRS_PER_UNIT: usize = 16_384;
const MAX_FILES_PER_UNIT: usize = 65_536;
const MAX_SPANS_TOTAL: usize = 4_000_000;

const DW_LNS_COPY: u8 = 1;
const DW_LNS_ADVANCE_PC: u8 = 2;
const DW_LNS_ADVANCE_LINE: u8 = 3;
const DW_LNS_SET_FILE: u8 = 4;
const DW_LNS_SET_COLUMN: u8 = 5;
const DW_LNS_NEGATE_STMT: u8 = 6;
const DW_LNS_SET_BASIC_BLOCK: u8 = 7;
const DW_LNS_CONST_ADD_PC: u8 = 8;
const DW_LNS_FIXED_ADVANCE_PC: u8 = 9;
const DW_LNS_SET_PROLOGUE_END: u8 = 10;
const DW_LNS_SET_EPILOGUE_BEGIN: u8 = 11;
const DW_LNS_SET_ISA: u8 = 12;

const DW_LNE_END_SEQUENCE: u8 = 1;
const DW_LNE_SET_ADDRESS: u8 = 2;
const DW_LNE_DEFINE_FILE: u8 = 3;
const DW_LNE_SET_DISCRIMINATOR: u8 = 4;

const DW_LNCT_PATH: u64 = 1;
const DW_LNCT_DIRECTORY_INDEX: u64 = 2;

const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA4: u64 = 0x06;
const DW_FORM_DATA8: u64 = 0x07;
const DW_FORM_STRING: u64 = 0x08;
const DW_FORM_DATA1: u64 = 0x0b;
const DW_FORM_FLAG: u64 = 0x0c;
const DW_FORM_STRP: u64 = 0x0e;
const DW_FORM_UDATA: u64 = 0x0f;
const DW_FORM_SDATA: u64 = 0x10;
const DW_FORM_SEC_OFFSET: u64 = 0x17;
const DW_FORM_FLAG_PRESENT: u64 = 0x19;
const DW_FORM_DATA16: u64 = 0x1e;
const DW_FORM_LINE_STRP: u64 = 0x1f;

/// Raw DWARF sections needed to decode `.debug_line` v2–v5.
#[derive(Clone, Copy, Default)]
pub(crate) struct DwarfLineSections<'a> {
    pub debug_line: &'a [u8],
    pub debug_line_str: &'a [u8],
    pub debug_str: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineSpan {
    start: u64,
    end: u64,
    file: u32,
    line: u32,
}

/// Link-time address ranges mapped to interned source file paths and lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LineTable {
    files: Vec<String>,
    spans: Vec<LineSpan>,
}

impl LineTable {
    /// Parse all compilation units in `sections.debug_line`.
    pub(crate) fn parse(sections: DwarfLineSections<'_>, default_addr_size: u8) -> Self {
        if sections.debug_line.is_empty()
            || sections.debug_line.len() as u64 > MAX_LINE_SECTION_BYTES
        {
            return Self::default();
        }
        let mut builder = TableBuilder::default();
        let mut cursor = Cursor::new(sections.debug_line);
        while !cursor.is_empty() && builder.spans.len() < MAX_SPANS_TOTAL {
            let Some((unit_bytes, is_dwarf64)) = read_unit_slice(&mut cursor) else {
                break;
            };
            let _ = parse_unit(
                unit_bytes,
                is_dwarf64,
                default_addr_size,
                sections,
                &mut builder,
            );
        }
        builder.finish()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Look up the source file path and 1-based line number for a link-time address.
    pub(crate) fn lookup(&self, unslid_addr: u64) -> Option<(&str, u32)> {
        let idx = self.spans.partition_point(|span| span.start <= unslid_addr);
        let span = self.spans.get(idx.checked_sub(1)?)?;
        if unslid_addr >= span.end || span.line == 0 {
            return None;
        }
        let file = self.files.get(span.file as usize)?;
        Some((file.as_str(), span.line))
    }
}

#[derive(Default)]
struct TableBuilder {
    files: Vec<String>,
    file_ids: HashMap<String, u32>,
    spans: Vec<LineSpan>,
}

impl TableBuilder {
    fn intern_file(&mut self, path: String) -> Option<u32> {
        if path.is_empty() {
            return None;
        }
        if let Some(&id) = self.file_ids.get(&path) {
            return Some(id);
        }
        let id = u32::try_from(self.files.len()).ok()?;
        self.files.push(path.clone());
        self.file_ids.insert(path, id);
        Some(id)
    }

    fn push_span(&mut self, start: u64, end: u64, file: u32, line: u32) {
        if self.spans.len() >= MAX_SPANS_TOTAL || start >= end || line == 0 {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.end == start
            && last.file == file
            && last.line == line
        {
            last.end = end;
            return;
        }
        self.spans.push(LineSpan {
            start,
            end,
            file,
            line,
        });
    }

    fn finish(mut self) -> LineTable {
        if self.spans.is_empty() {
            return LineTable::default();
        }
        self.spans
            .sort_by(|left, right| left.start.cmp(&right.start).then(left.end.cmp(&right.end)));
        let mut merged: Vec<LineSpan> = Vec::with_capacity(self.spans.len());
        for span in self.spans {
            if let Some(prev) = merged.last_mut() {
                if span.start < prev.end {
                    prev.end = span.start;
                }
                if prev.start >= prev.end {
                    *prev = span;
                    continue;
                }
                if prev.end == span.start && prev.file == span.file && prev.line == span.line {
                    prev.end = span.end;
                    continue;
                }
            }
            if span.start < span.end {
                merged.push(span);
            }
        }
        LineTable {
            files: self.files,
            spans: merged,
        }
    }
}

fn read_unit_slice<'a>(cursor: &mut Cursor<'a>) -> Option<(&'a [u8], bool)> {
    let first = cursor.read_u32()?;
    if first == 0 {
        return None;
    }
    if first == 0xffff_ffff {
        let len = usize::try_from(cursor.read_u64()?).ok()?;
        let slice = cursor.read_slice(len)?;
        Some((slice, true))
    } else if first < 0xffff_fff0 {
        let slice = cursor.read_slice(first as usize)?;
        Some((slice, false))
    } else {
        None
    }
}

fn parse_unit(
    unit_bytes: &[u8],
    is_dwarf64: bool,
    default_addr_size: u8,
    sections: DwarfLineSections<'_>,
    builder: &mut TableBuilder,
) -> Option<()> {
    let mut header = Cursor::new(unit_bytes);
    let version = header.read_u16()?;
    if !(2..=5).contains(&version) {
        return None;
    }
    let addr_size = if version >= 5 {
        let addr_size = header.read_u8()?;
        let _seg_size = header.read_u8()?;
        addr_size
    } else {
        default_addr_size
    };
    if addr_size != 4 && addr_size != 8 {
        return None;
    }
    let offset_size: u8 = if is_dwarf64 { 8 } else { 4 };
    let header_len = usize::try_from(header.read_offset(offset_size)?).ok()?;
    let after_header_len = header.remaining();
    if header_len > after_header_len.len() {
        return None;
    }
    let program_bytes = &after_header_len[header_len..];
    let mut hdr = Cursor::new(&after_header_len[..header_len]);

    let min_inst_len = hdr.read_u8()?;
    if min_inst_len == 0 {
        return None;
    }
    let max_ops_per_inst = if version >= 4 { hdr.read_u8()? } else { 1 };
    if max_ops_per_inst == 0 {
        return None;
    }
    let default_is_stmt = hdr.read_u8()? != 0;
    let line_base = hdr.read_u8()? as i8;
    let line_range = hdr.read_u8()?;
    let opcode_base = hdr.read_u8()?;
    if line_range == 0 || opcode_base == 0 {
        return None;
    }
    let std_opcode_lengths = hdr.read_slice(usize::from(opcode_base - 1))?;

    let mut dirs: Vec<String> = Vec::new();
    let mut unit_files: Vec<Option<u32>> = Vec::new();
    if version <= 4 {
        dirs.push(String::new());
        while !hdr.is_empty() {
            let dir = hdr.read_cstr()?;
            if dir.is_empty() {
                break;
            }
            if dirs.len() >= MAX_DIRS_PER_UNIT {
                return None;
            }
            dirs.push(dir.to_string());
        }
        // DWARF v2–v4 file numbers are 1-based; slot 0 is unused.
        unit_files.push(None);
        while !hdr.is_empty() {
            let name = hdr.read_cstr()?;
            if name.is_empty() {
                break;
            }
            let dir_idx = usize::try_from(hdr.read_uleb128()?).ok()?;
            let _mtime = hdr.read_uleb128()?;
            let _size = hdr.read_uleb128()?;
            if unit_files.len() >= MAX_FILES_PER_UNIT {
                return None;
            }
            let dir = dirs.get(dir_idx).map(String::as_str).unwrap_or("");
            let joined = join_dwarf_path(dir, name);
            unit_files.push(builder.intern_file(joined));
        }
    } else {
        let dir_fmt = read_v5_format(&mut hdr)?;
        let dir_count = usize::try_from(hdr.read_uleb128()?).ok()?;
        if dir_count > MAX_DIRS_PER_UNIT {
            return None;
        }
        for _ in 0..dir_count {
            let (path, _) = read_v5_entry(&mut hdr, &dir_fmt, offset_size, sections)?;
            dirs.push(path.unwrap_or_default());
        }
        let file_fmt = read_v5_format(&mut hdr)?;
        let file_count = usize::try_from(hdr.read_uleb128()?).ok()?;
        if file_count > MAX_FILES_PER_UNIT {
            return None;
        }
        for _ in 0..file_count {
            let (path, dir_idx) = read_v5_entry(&mut hdr, &file_fmt, offset_size, sections)?;
            let id = path.and_then(|name| {
                let dir = dir_idx
                    .and_then(|idx| usize::try_from(idx).ok())
                    .and_then(|idx| dirs.get(idx))
                    .map(String::as_str)
                    .unwrap_or("");
                builder.intern_file(join_dwarf_path(dir, &name))
            });
            unit_files.push(id);
        }
    }

    run_program(
        program_bytes,
        ProgramConfig {
            version,
            addr_size,
            min_inst_len,
            max_ops_per_inst,
            default_is_stmt,
            line_base,
            line_range,
            opcode_base,
            std_opcode_lengths,
        },
        &dirs,
        &mut unit_files,
        builder,
    );
    Some(())
}

fn read_v5_format(hdr: &mut Cursor<'_>) -> Option<Vec<(u64, u64)>> {
    let count = usize::from(hdr.read_u8()?);
    let mut fmt = Vec::with_capacity(count);
    for _ in 0..count {
        let content_type = hdr.read_uleb128()?;
        let form = hdr.read_uleb128()?;
        fmt.push((content_type, form));
    }
    Some(fmt)
}

fn read_v5_entry(
    hdr: &mut Cursor<'_>,
    fmt: &[(u64, u64)],
    offset_size: u8,
    sections: DwarfLineSections<'_>,
) -> Option<(Option<String>, Option<u64>)> {
    let mut path = None;
    let mut dir_idx = None;
    for &(content_type, form) in fmt {
        match form {
            DW_FORM_STRING => {
                let text = hdr.read_cstr()?;
                if content_type == DW_LNCT_PATH {
                    path = Some(text.to_string());
                }
            }
            DW_FORM_LINE_STRP => {
                let off = usize::try_from(hdr.read_offset(offset_size)?).ok()?;
                let text = cstr_at(sections.debug_line_str, off)?;
                if content_type == DW_LNCT_PATH {
                    path = Some(text.to_string());
                }
            }
            DW_FORM_STRP => {
                let off = usize::try_from(hdr.read_offset(offset_size)?).ok()?;
                let text = cstr_at(sections.debug_str, off)?;
                if content_type == DW_LNCT_PATH {
                    path = Some(text.to_string());
                }
            }
            DW_FORM_UDATA => {
                let val = hdr.read_uleb128()?;
                if content_type == DW_LNCT_DIRECTORY_INDEX {
                    dir_idx = Some(val);
                }
            }
            DW_FORM_DATA1 => {
                let val = u64::from(hdr.read_u8()?);
                if content_type == DW_LNCT_DIRECTORY_INDEX {
                    dir_idx = Some(val);
                }
            }
            DW_FORM_DATA2 => {
                let val = u64::from(hdr.read_u16()?);
                if content_type == DW_LNCT_DIRECTORY_INDEX {
                    dir_idx = Some(val);
                }
            }
            DW_FORM_DATA4 => {
                let val = u64::from(hdr.read_u32()?);
                if content_type == DW_LNCT_DIRECTORY_INDEX {
                    dir_idx = Some(val);
                }
            }
            DW_FORM_DATA8 => {
                let val = hdr.read_u64()?;
                if content_type == DW_LNCT_DIRECTORY_INDEX {
                    dir_idx = Some(val);
                }
            }
            DW_FORM_DATA16 => {
                let _ = hdr.read_slice(16)?;
            }
            DW_FORM_SDATA => {
                let _ = hdr.read_sleb128()?;
            }
            DW_FORM_FLAG => {
                let _ = hdr.read_u8()?;
            }
            DW_FORM_FLAG_PRESENT => {}
            DW_FORM_SEC_OFFSET => {
                let _ = hdr.read_offset(offset_size)?;
            }
            _ => return None,
        }
    }
    Some((path, dir_idx))
}

struct ProgramConfig<'a> {
    version: u16,
    addr_size: u8,
    min_inst_len: u8,
    max_ops_per_inst: u8,
    default_is_stmt: bool,
    line_base: i8,
    line_range: u8,
    opcode_base: u8,
    std_opcode_lengths: &'a [u8],
}

struct Registers {
    address: u64,
    op_index: u64,
    file: u64,
    line: i64,
    is_stmt: bool,
    tombstone: bool,
}

impl Registers {
    fn new(default_is_stmt: bool) -> Self {
        Self {
            address: 0,
            op_index: 0,
            file: 1,
            line: 1,
            is_stmt: default_is_stmt,
            tombstone: false,
        }
    }

    fn advance(&mut self, op_advance: u64, min_inst_len: u8, max_ops_per_inst: u8) {
        if max_ops_per_inst <= 1 {
            self.address = self
                .address
                .wrapping_add(op_advance.saturating_mul(u64::from(min_inst_len)));
        } else {
            let max_ops = u64::from(max_ops_per_inst);
            let total = self.op_index.saturating_add(op_advance);
            let inst_delta = (total / max_ops).saturating_mul(u64::from(min_inst_len));
            self.address = self.address.wrapping_add(inst_delta);
            self.op_index = total % max_ops;
        }
    }
}

fn is_tombstone_address(addr: u64, addr_size: u8) -> bool {
    if addr == 0 {
        return true;
    }
    if addr_size == 4 {
        addr >= u64::from(u32::MAX - 1)
    } else {
        addr >= u64::MAX - 0x1000
    }
}

fn run_program(
    program_bytes: &[u8],
    cfg: ProgramConfig<'_>,
    dirs: &[String],
    unit_files: &mut Vec<Option<u32>>,
    builder: &mut TableBuilder,
) {
    let mut prog = Cursor::new(program_bytes);
    let mut regs = Registers::new(cfg.default_is_stmt);
    let mut prev_row: Option<(u64, Option<u32>, u32)> = None;

    let emit_row = |regs: &Registers,
                    end_seq: bool,
                    unit_files: &[Option<u32>],
                    prev_row: &mut Option<(u64, Option<u32>, u32)>,
                    builder: &mut TableBuilder| {
        if regs.tombstone {
            if end_seq {
                *prev_row = None;
            }
            return;
        }
        if let Some((prev_addr, Some(prev_file), prev_line)) = *prev_row
            && prev_addr > 0
            && regs.address > prev_addr
            && prev_line > 0
        {
            builder.push_span(prev_addr, regs.address, prev_file, prev_line);
        }
        if end_seq {
            *prev_row = None;
        } else {
            let file_id = usize::try_from(regs.file)
                .ok()
                .and_then(|idx| unit_files.get(idx).copied().flatten());
            let line = u32::try_from(regs.line.max(0)).unwrap_or(0);
            *prev_row = Some((regs.address, file_id, line));
        }
    };

    while !prog.is_empty() && builder.spans.len() < MAX_SPANS_TOTAL {
        let Some(opcode) = prog.read_u8() else {
            break;
        };
        if opcode == 0 {
            let Some(ext_len) = prog.read_uleb128().and_then(|n| usize::try_from(n).ok()) else {
                break;
            };
            let Some(ext_bytes) = prog.read_slice(ext_len) else {
                break;
            };
            let mut ext = Cursor::new(ext_bytes);
            let Some(sub) = ext.read_u8() else {
                continue;
            };
            match sub {
                DW_LNE_END_SEQUENCE => {
                    emit_row(&regs, true, unit_files, &mut prev_row, builder);
                    regs = Registers::new(cfg.default_is_stmt);
                }
                DW_LNE_SET_ADDRESS => {
                    let Some(addr) = ext.read_addr(cfg.addr_size) else {
                        break;
                    };
                    regs.address = addr;
                    regs.op_index = 0;
                    regs.tombstone = is_tombstone_address(addr, cfg.addr_size);
                }
                DW_LNE_DEFINE_FILE if cfg.version <= 4 => {
                    if let Some(name) = ext.read_cstr()
                        && !name.is_empty()
                        && let Some(dir_idx) = ext.read_uleb128()
                        && ext.read_uleb128().is_some()
                        && ext.read_uleb128().is_some()
                        && unit_files.len() < MAX_FILES_PER_UNIT
                    {
                        let dir = usize::try_from(dir_idx)
                            .ok()
                            .and_then(|idx| dirs.get(idx))
                            .map(String::as_str)
                            .unwrap_or("");
                        let id = builder.intern_file(join_dwarf_path(dir, name));
                        unit_files.push(id);
                    }
                }
                DW_LNE_SET_DISCRIMINATOR => {
                    let _ = ext.read_uleb128();
                }
                _ => {}
            }
        } else if opcode >= cfg.opcode_base {
            let adjusted = u64::from(opcode - cfg.opcode_base);
            let line_range = u64::from(cfg.line_range);
            let op_advance = adjusted / line_range;
            let line_inc = i64::from(cfg.line_base) + (adjusted % line_range) as i64;
            regs.advance(op_advance, cfg.min_inst_len, cfg.max_ops_per_inst);
            regs.line = regs.line.saturating_add(line_inc);
            emit_row(&regs, false, unit_files, &mut prev_row, builder);
        } else {
            match opcode {
                DW_LNS_COPY => {
                    emit_row(&regs, false, unit_files, &mut prev_row, builder);
                }
                DW_LNS_ADVANCE_PC => {
                    let Some(adv) = prog.read_uleb128() else {
                        break;
                    };
                    regs.advance(adv, cfg.min_inst_len, cfg.max_ops_per_inst);
                }
                DW_LNS_ADVANCE_LINE => {
                    let Some(delta) = prog.read_sleb128() else {
                        break;
                    };
                    regs.line = regs.line.saturating_add(delta);
                }
                DW_LNS_SET_FILE => {
                    let Some(file) = prog.read_uleb128() else {
                        break;
                    };
                    regs.file = file;
                }
                DW_LNS_SET_COLUMN => {
                    if prog.read_uleb128().is_none() {
                        break;
                    }
                }
                DW_LNS_NEGATE_STMT => {
                    regs.is_stmt = !regs.is_stmt;
                }
                DW_LNS_SET_BASIC_BLOCK | DW_LNS_SET_PROLOGUE_END | DW_LNS_SET_EPILOGUE_BEGIN => {}
                DW_LNS_CONST_ADD_PC => {
                    let adjusted = u64::from(255 - cfg.opcode_base);
                    let op_advance = adjusted / u64::from(cfg.line_range);
                    regs.advance(op_advance, cfg.min_inst_len, cfg.max_ops_per_inst);
                }
                DW_LNS_FIXED_ADVANCE_PC => {
                    let Some(delta) = prog.read_u16() else {
                        break;
                    };
                    regs.address = regs.address.wrapping_add(u64::from(delta));
                    regs.op_index = 0;
                }
                DW_LNS_SET_ISA => {
                    if prog.read_uleb128().is_none() {
                        break;
                    }
                }
                other => {
                    let count = cfg
                        .std_opcode_lengths
                        .get(usize::from(other - 1))
                        .copied()
                        .unwrap_or(0);
                    let mut ok = true;
                    for _ in 0..count {
                        if prog.read_uleb128().is_none() {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        break;
                    }
                }
            }
        }
    }
}

fn join_dwarf_path(dir: &str, name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    let dir = dir.trim();
    if dir.is_empty() || is_rooted_path(name) {
        return clean_dot_slash(name).to_string();
    }
    let sep = if dir.contains('\\') && !dir.contains('/') {
        '\\'
    } else {
        '/'
    };
    let dir = dir.trim_end_matches(['/', '\\']);
    let rel = clean_dot_slash(name);
    if rel.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}{sep}{rel}")
    }
}

fn clean_dot_slash(path: &str) -> &str {
    let mut cur = path;
    while let Some(rest) = cur.strip_prefix("./").or_else(|| cur.strip_prefix(".\\")) {
        cur = rest;
    }
    cur
}

fn is_rooted_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.first().is_some_and(|b| *b == b'/' || *b == b'\\') {
        return true;
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn cstr_at(buf: &[u8], offset: usize) -> Option<&str> {
    let rest = buf.get(offset..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&rest[..end]).ok()
}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn remaining(&self) -> &'a [u8] {
        self.bytes
    }

    fn read_slice(&mut self, len: usize) -> Option<&'a [u8]> {
        if len > self.bytes.len() {
            return None;
        }
        let (head, tail) = self.bytes.split_at(len);
        self.bytes = tail;
        Some(head)
    }

    fn read_u8(&mut self) -> Option<u8> {
        let (&byte, tail) = self.bytes.split_first()?;
        self.bytes = tail;
        Some(byte)
    }

    fn read_u16(&mut self) -> Option<u16> {
        let raw = self.read_slice(2)?;
        Some(u16::from_le_bytes([raw[0], raw[1]]))
    }

    fn read_u32(&mut self) -> Option<u32> {
        let raw = self.read_slice(4)?;
        Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let raw = self.read_slice(8)?;
        Some(u64::from_le_bytes(raw.try_into().ok()?))
    }

    fn read_offset(&mut self, offset_size: u8) -> Option<u64> {
        match offset_size {
            4 => self.read_u32().map(u64::from),
            8 => self.read_u64(),
            _ => None,
        }
    }

    fn read_addr(&mut self, addr_size: u8) -> Option<u64> {
        self.read_offset(addr_size)
    }

    fn read_cstr(&mut self) -> Option<&'a str> {
        let end = self.bytes.iter().position(|&b| b == 0)?;
        let text = std::str::from_utf8(&self.bytes[..end]).ok()?;
        self.bytes = &self.bytes[end + 1..];
        Some(text)
    }

    fn read_uleb128(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        for _ in 0..10 {
            let byte = self.read_u8()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return None;
            }
            result |= low << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
        }
        None
    }

    fn read_sleb128(&mut self) -> Option<i64> {
        let mut result = 0i64;
        let mut shift = 0u32;
        let mut byte;
        for _ in 0..10 {
            byte = self.read_u8()?;
            let low = i64::from(byte & 0x7f);
            if shift == 63 && byte != 0x00 && byte != 0x7f {
                return None;
            }
            result |= low << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && (byte & 0x40) != 0 {
                    result |= !0i64 << shift;
                }
                return Some(result);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DW_FORM_DATA1, DW_FORM_DATA16, DW_FORM_LINE_STRP, DW_LNCT_DIRECTORY_INDEX, DW_LNCT_PATH,
        DW_LNE_END_SEQUENCE, DW_LNE_SET_ADDRESS, DW_LNS_ADVANCE_LINE, DW_LNS_ADVANCE_PC,
        DW_LNS_COPY, DW_LNS_SET_FILE, DwarfLineSections, LineTable,
    };

    #[test]
    fn parses_dwarf_v4_line_program_and_joins_directory() {
        let mut header = vec![
            1,            // min_inst_len
            1,            // max_ops_per_inst
            1,            // default_is_stmt
            (-5i8) as u8, // line_base = -5
            14,           // line_range = 14
            13,           // opcode_base = 13
        ];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        // include_directories[1] = "/workspace/src"
        header.extend_from_slice(b"/workspace/src\0\0");
        // file_names[1] = "spin.cpp" in dir 1
        header.extend_from_slice(b"spin.cpp\0");
        header.extend_from_slice(&[1, 0, 0]);
        header.push(0);

        let mut prog = Vec::new();
        // DW_LNE_SET_ADDRESS 0x1000
        prog.extend_from_slice(&[0, 9, DW_LNE_SET_ADDRESS]);
        prog.extend_from_slice(&0x1000u64.to_le_bytes());
        // DW_LNS_ADVANCE_LINE +9 -> line 10, DW_LNS_COPY -> row at 0x1000 line 10
        prog.extend_from_slice(&[DW_LNS_ADVANCE_LINE, 9, DW_LNS_COPY]);
        // Special opcode: op_advance = 0x10 (16), line_inc = +2 -> line 12 at 0x1010
        // adjusted = 16 * 14 + (2 - (-5)) = 224 + 7 = 231; opcode = 13 + 231 = 244
        prog.push(244);
        // DW_LNS_ADVANCE_PC +0x20 -> 0x1030, then DW_LNE_END_SEQUENCE
        prog.extend_from_slice(&[DW_LNS_ADVANCE_PC, 0x20]);
        prog.extend_from_slice(&[0, 1, DW_LNE_END_SEQUENCE]);

        let mut unit = Vec::new();
        unit.extend_from_slice(&4u16.to_le_bytes());
        unit.extend_from_slice(&(header.len() as u32).to_le_bytes());
        unit.extend_from_slice(&header);
        unit.extend_from_slice(&prog);

        let mut debug_line = Vec::new();
        debug_line.extend_from_slice(&(unit.len() as u32).to_le_bytes());
        debug_line.extend_from_slice(&unit);

        let table = LineTable::parse(
            DwarfLineSections {
                debug_line: &debug_line,
                ..Default::default()
            },
            8,
        );
        assert!(!table.is_empty());
        assert_eq!(table.lookup(0x0fff), None);
        assert_eq!(table.lookup(0x1000), Some(("/workspace/src/spin.cpp", 10)));
        assert_eq!(table.lookup(0x100f), Some(("/workspace/src/spin.cpp", 10)));
        assert_eq!(table.lookup(0x1010), Some(("/workspace/src/spin.cpp", 12)));
        assert_eq!(table.lookup(0x102f), Some(("/workspace/src/spin.cpp", 12)));
        assert_eq!(table.lookup(0x1030), None);
    }

    #[test]
    fn parses_dwarf_v5_with_line_strp_and_md5_data16() {
        let mut line_str = Vec::new();
        let dir_off = line_str.len() as u32;
        line_str.extend_from_slice(b"/repo/crates\0");
        let file_off = line_str.len() as u32;
        line_str.extend_from_slice(b"./main.rs\0");

        let mut header = vec![1, 1, 1, (-5i8) as u8, 14, 13];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        // directory_entry_format: [(DW_LNCT_PATH, DW_FORM_LINE_STRP)]
        header.extend_from_slice(&[1, DW_LNCT_PATH as u8, DW_FORM_LINE_STRP as u8]);
        header.push(1); // directories_count = 1
        header.extend_from_slice(&dir_off.to_le_bytes());
        // file_name_entry_format: [(PATH, LINE_STRP), (DIR_INDEX, DATA1), (MD5=5, DATA16)]
        header.extend_from_slice(&[
            3,
            DW_LNCT_PATH as u8,
            DW_FORM_LINE_STRP as u8,
            DW_LNCT_DIRECTORY_INDEX as u8,
            DW_FORM_DATA1 as u8,
            5,
            DW_FORM_DATA16 as u8,
        ]);
        header.push(1); // file_names_count = 1 (0-based index 0!)
        header.extend_from_slice(&file_off.to_le_bytes());
        header.push(0); // dir index 0
        header.extend_from_slice(&[0xaa; 16]); // MD5

        let mut prog = Vec::new();
        // Tombstone sequence at 0 must be ignored.
        prog.extend_from_slice(&[0, 9, DW_LNE_SET_ADDRESS]);
        prog.extend_from_slice(&0u64.to_le_bytes());
        prog.extend_from_slice(&[DW_LNS_SET_FILE, 0, DW_LNS_ADVANCE_LINE, 40, DW_LNS_COPY]);
        prog.extend_from_slice(&[DW_LNS_ADVANCE_PC, 0x10, 0, 1, DW_LNE_END_SEQUENCE]);

        // Live sequence at 0x2000..0x2020 -> file 0, line 42
        prog.extend_from_slice(&[0, 9, DW_LNE_SET_ADDRESS]);
        prog.extend_from_slice(&0x2000u64.to_le_bytes());
        prog.extend_from_slice(&[DW_LNS_SET_FILE, 0, DW_LNS_ADVANCE_LINE, 41, DW_LNS_COPY]);
        prog.extend_from_slice(&[DW_LNS_ADVANCE_PC, 0x20, 0, 1, DW_LNE_END_SEQUENCE]);

        let mut unit = Vec::new();
        unit.extend_from_slice(&5u16.to_le_bytes());
        unit.push(8); // address_size
        unit.push(0); // segment_selector_size
        unit.extend_from_slice(&(header.len() as u32).to_le_bytes());
        unit.extend_from_slice(&header);
        unit.extend_from_slice(&prog);

        let mut debug_line = Vec::new();
        debug_line.extend_from_slice(&(unit.len() as u32).to_le_bytes());
        debug_line.extend_from_slice(&unit);

        let table = LineTable::parse(
            DwarfLineSections {
                debug_line: &debug_line,
                debug_line_str: &line_str,
                debug_str: &[],
            },
            8,
        );
        assert_eq!(table.lookup(0x0004), None);
        assert_eq!(table.lookup(0x2008), Some(("/repo/crates/main.rs", 42)));
        assert_eq!(table.lookup(0x2020), None);
    }

    #[test]
    fn truncated_or_garbage_dwarf_returns_empty_table() {
        assert!(LineTable::parse(DwarfLineSections::default(), 8).is_empty());
        assert!(
            LineTable::parse(
                DwarfLineSections {
                    debug_line: &[0xff, 0x00, 0x12, 0x34, 0x56],
                    ..Default::default()
                },
                8,
            )
            .is_empty()
        );
    }
}
