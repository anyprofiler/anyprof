//! Display names for C, C++, and Rust symbols.
//!
//! Rust uses its own mangling (legacy `_ZN…17h<hex>E` and v0 `_R…`). Those
//! names are demangled with `rustc_demangle` before Itanium C++ demangling,
//! because a legacy Rust symbol is also a legal Itanium name and
//! `cpp_demangle` would keep the crate hash. v0 names are not Itanium.
//! Mach-O adds one extra leading underscore; ELF and PE do not.
//!
//! A symbol this module recognizes as Rust is reported as
//! [`anyprof_core::Runtime::Rust`]. Everything else stays
//! [`anyprof_core::Runtime::Native`], including a C++ symbol that happens to
//! share the Itanium prefix.

use anyprof_core::Runtime;

/// How a symbol string should be shown in a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Presented {
    /// Demangled name, or the raw name when it is not mangled.
    pub name: String,
    /// [`Runtime::Rust`] when `name` came from Rust mangling.
    pub runtime: Runtime,
    /// `cpp_demangle` produced `name`, or the raw symbol was MSVC `?` mangling.
    /// A Rust image must not relabel that frame.
    pub from_cpp: bool,
}

/// ELF, PE, and dbghelp names. A GNU version suffix (`@@GLIBC_2.2.5`) is removed.
pub(crate) fn present_elf_symbol(raw: &str) -> Presented {
    let trimmed = strip_gnu_version(raw);
    if let Some(name) = demangle_rust(trimmed) {
        return rust(name);
    }
    let candidate = if let Some(rest) = trimmed.strip_prefix("__Z") {
        format!("_Z{rest}")
    } else {
        trimmed.to_string()
    };
    if let Some(name) = demangle_cpp(&candidate) {
        return cpp(name);
    }
    if candidate == raw {
        native(raw.to_string())
    } else {
        native(candidate)
    }
}

/// Mach-O `nlist` names. The extra underscore is stripped for C symbols
/// (`_main` → `main`) and for Rust and Itanium names (`__R…`, `__Z…`).
pub(crate) fn present_macho_symbol(raw: &str) -> Presented {
    if let Some(name) = demangle_rust(raw) {
        return rust(name);
    }
    if let Some(stripped) = raw.strip_prefix('_')
        && let Some(name) = demangle_rust(stripped)
    {
        return rust(name);
    }
    let itanium = if let Some(rest) = raw.strip_prefix("__Z") {
        Some(format!("_Z{rest}"))
    } else if raw.starts_with("_Z") {
        Some(raw.to_string())
    } else {
        None
    };
    if let Some(candidate) = itanium
        && let Some(name) = demangle_cpp(&candidate)
    {
        return cpp(name);
    }
    match raw.strip_prefix('_') {
        Some(rest) if !rest.starts_with('_') => native(rest.to_string()),
        _ => native(raw.to_string()),
    }
}

/// Runtime to store on a frame.
///
/// Demangled Rust mangling wins. On Windows, dbghelp can hand back an
/// already-undecorated Rust path (`crate::function`) with no mangled form
/// left to recognize. That path is labeled Rust when the file is a Rust
/// image. Itanium C++ demangles to `ns::fn()` and is marked `from_cpp`, so
/// it stays native inside that image. An MSVC `?` name is also `from_cpp`.
/// A C symbol (`main`) has no `::` and stays native.
pub(crate) fn frame_runtime(
    symbol: Runtime,
    name: &str,
    from_cpp: bool,
    image_is_rust: bool,
) -> Runtime {
    if symbol == Runtime::Rust {
        return Runtime::Rust;
    }
    if from_cpp || !image_is_rust || !undecorated_rust_path(name) {
        return Runtime::Native;
    }
    Runtime::Rust
}

/// `crate::item` as dbghelp stores it after dropping Rust mangling.
///
/// Itanium demangling keeps a parameter list (`ns::fn()`). A signature,
/// template argument, or space is not this fallback: `foo::<T>` and
/// `<T as Trait>` stay native so a C++ template in the same image is not
/// relabeled. A mangled closure or generic still goes through
/// `rustc-demangle` and is labeled Rust before this check. rustc's
/// already-printed `{closure#0}` and `{impl#0}` segments are Rust-only, so
/// those are accepted here.
fn undecorated_rust_path(name: &str) -> bool {
    let mut parts = name.split("::");
    let Some(first) = parts.next() else {
        return false;
    };
    if !rust_ident(first) || parts.next().is_none() {
        return false;
    }
    name.split("::").all(rust_path_component)
}

fn rust_ident(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An identifier, or a rustc path segment such as `{closure#0}` / `{impl#1}`.
fn rust_path_component(part: &str) -> bool {
    if rust_ident(part) {
        return true;
    }
    let Some(inner) = part
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    let Some((kind, index)) = inner.split_once('#') else {
        return false;
    };
    rust_ident(kind) && !index.is_empty() && index.chars().all(|c| c.is_ascii_digit())
}

fn rust(name: String) -> Presented {
    Presented {
        name,
        runtime: Runtime::Rust,
        from_cpp: false,
    }
}

fn native(name: String) -> Presented {
    Presented {
        name,
        runtime: Runtime::Native,
        from_cpp: false,
    }
}

fn cpp(name: String) -> Presented {
    Presented {
        name,
        runtime: Runtime::Native,
        from_cpp: true,
    }
}

fn strip_gnu_version(raw: &str) -> &str {
    raw.split("@@").next().unwrap_or(raw)
}

/// Pretty Rust name, with the legacy crate hash omitted.
///
/// `try_demangle` rejects strings that are not Rust, including ordinary
/// Itanium C++ (`_ZN14anyprof_native4spinEv` has no Rust hash).
fn demangle_rust(raw: &str) -> Option<String> {
    let demangled = rustc_demangle::try_demangle(raw).ok()?;
    let pretty = format!("{demangled:#}");
    if pretty.is_empty() || pretty == raw {
        None
    } else {
        Some(pretty)
    }
}

fn demangle_cpp(raw: &str) -> Option<String> {
    let symbol = cpp_demangle::Symbol::new(raw).ok()?;
    let text = symbol.to_string();
    if text.is_empty() || text == raw {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::{frame_runtime, present_elf_symbol, present_macho_symbol};
    use anyprof_core::Runtime;

    /// Real v0 symbol from `rustc 1.99` (`--crate-name anyprof_rust_fixture`).
    const V0_SPIN: &str = "_RNvCs2EeUBRI4hGO_20anyprof_rust_fixture17anyprof_rust_spin";

    #[test]
    fn v0_and_legacy_rust_names_are_demangled_without_the_hash() {
        let v0 = present_elf_symbol(V0_SPIN);
        assert_eq!(v0.runtime, Runtime::Rust);
        assert_eq!(v0.name, "anyprof_rust_fixture::anyprof_rust_spin");

        let legacy = present_elf_symbol("_ZN3foo3bar17h0123456789abcdefE");
        assert_eq!(legacy.runtime, Runtime::Rust);
        assert_eq!(legacy.name, "foo::bar");
        assert!(!legacy.name.contains("0123456789abcdef"), "{}", legacy.name);
    }

    #[test]
    fn itanium_cpp_is_not_labeled_rust() {
        let pretty = present_elf_symbol("_ZN14anyprof_native4spinEv");
        assert_eq!(pretty.runtime, Runtime::Native);
        assert!(
            pretty.name.contains("anyprof_native") && pretty.name.contains("spin"),
            "{}",
            pretty.name
        );
        assert_eq!(present_elf_symbol("main").name, "main");
        assert_eq!(present_elf_symbol("main").runtime, Runtime::Native);
        assert_eq!(present_elf_symbol("read@@GLIBC_2.2.5").name, "read");
    }

    #[test]
    fn macho_strips_one_underscore_and_demangles_rust() {
        let rust = present_macho_symbol(&format!("_{V0_SPIN}"));
        assert_eq!(rust.runtime, Runtime::Rust);
        assert_eq!(rust.name, "anyprof_rust_fixture::anyprof_rust_spin");

        let cpp = present_macho_symbol("__ZN14anyprof_native4spinEv");
        assert_eq!(cpp.runtime, Runtime::Native);
        assert!(cpp.name.contains("spin"), "{}", cpp.name);

        assert_eq!(present_macho_symbol("_main").name, "main");
        assert_eq!(present_macho_symbol("main").name, "main");
        assert_eq!(
            present_macho_symbol("__stack_chk_fail").name,
            "__stack_chk_fail"
        );
    }

    #[test]
    fn a_rust_image_labels_an_already_demangled_path() {
        assert_eq!(
            frame_runtime(
                Runtime::Native,
                "anyprof_rust_fixture::anyprof_rust_spin",
                false,
                true
            ),
            Runtime::Rust
        );
        assert_eq!(
            frame_runtime(Runtime::Native, "main", false, true),
            Runtime::Native
        );
        assert_eq!(
            frame_runtime(Runtime::Native, "anyprof_native::spin", false, false),
            Runtime::Native
        );
        assert_eq!(
            frame_runtime(
                Runtime::Rust,
                "anyprof_rust_fixture::anyprof_rust_spin",
                false,
                false
            ),
            Runtime::Rust
        );
    }

    #[test]
    fn cpp_inside_a_rust_image_stays_native() {
        let itanium = present_elf_symbol("_ZN2ns2fnEv");
        assert!(itanium.from_cpp, "{}", itanium.name);
        assert!(
            itanium.name.contains('('),
            "Itanium demangling keeps a parameter list: {}",
            itanium.name
        );
        assert_eq!(itanium.runtime, Runtime::Native);
        assert_eq!(
            frame_runtime(itanium.runtime, &itanium.name, false, true),
            Runtime::Native,
            "the parentheses alone keep a C++ demangle native"
        );
        assert_eq!(
            frame_runtime(itanium.runtime, &itanium.name, itanium.from_cpp, true),
            Runtime::Native
        );
        let macho = present_macho_symbol("__ZN2ns2fnEv");
        assert!(macho.from_cpp, "{}", macho.name);
        assert_eq!(
            frame_runtime(macho.runtime, &macho.name, macho.from_cpp, true),
            Runtime::Native
        );
        // dbghelp undecorates MSVC `?` names to `ns::fn` with no parameter list.
        assert_eq!(
            frame_runtime(Runtime::Native, "anyprof_native::spin", true, true),
            Runtime::Native
        );
    }

    #[test]
    fn closure_segments_are_rust_and_generics_are_not_this_fallback() {
        assert_eq!(
            frame_runtime(
                Runtime::Native,
                "anyprof_rust_fixture::spin::{closure#0}",
                false,
                true
            ),
            Runtime::Rust
        );
        assert_eq!(
            frame_runtime(
                Runtime::Native,
                "anyprof_rust_fixture::{impl#1}::call",
                false,
                true
            ),
            Runtime::Rust
        );
        assert_eq!(
            frame_runtime(Runtime::Native, "foo::bar::<i32>", false, true),
            Runtime::Native
        );
        assert_eq!(
            frame_runtime(
                Runtime::Native,
                "<core::slice::Iter as Iterator>::next",
                false,
                true
            ),
            Runtime::Native
        );
        let mangled = present_elf_symbol("_RNCNCNgCs6DXkGYLi8lr_2cc5spawn00B5_");
        assert_eq!(mangled.runtime, Runtime::Rust);
        assert!(mangled.name.contains("{closure#0}"), "{}", mangled.name);
    }
}
