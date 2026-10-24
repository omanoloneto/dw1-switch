//! Overlay data lifetimes (build side): each load of an overlay on the PS1 copies its file over
//! its load address, so its initialised data (`.data`, in the file) starts from the file's values
//! every time, while its `.bss` (not in the file) keeps whatever the RAM held, and the small
//! variables CodeWarrior put in the main executable's `.sdata` (resident) keep their values across
//! loads. Natively the overlays' C is linked once, so this module arranges the same:
//!
//! - every overlay C file whose `.data` the original `overlay.ld` places in an overlay is compiled
//!   through a generated wrapper that puts the file's data in two sections of its own per overlay:
//!   initialised data in `__DATA,__dw1_<u>`, zero-initialised and uninitialised data in
//!   `__DATA,__dw1_<u>z`;
//! - the wrapper also lists the variables a reload must not touch, with their native sizes: the
//!   file's resident variables (names the main symbol list places in the main executable, at
//!   0x80090000 and up) and its definitions without an initialiser (the PS1's overlay `.bss`,
//!   which a load doesn't clear). The definitions come from the preprocessed file
//!   (`top_level_definitions`), so ones built by macros count;
//! - `src/overlays.rs` (generated here as `overlays_table.rs`) snapshots the first section before
//!   the game starts and, when the game loads an overlay (`loadDynamicLibrary`), puts the
//!   snapshot back and zeroes the second, keeping the listed variables' current values.
//!
//! So an explicit zero initialiser resets like any other, as on the PS1 (CodeWarrior put those in
//! `.data`; the `*_bss_us.c` files are all of that kind). Function-local statics can't be listed
//! from the wrapper; the build stops if an overlay data file has one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Units that are overlays, with their `Overlay` number (`REL_BIN_FILES` order in utils2.c:
/// BTL is 1 ... EAB is 16; SHOP, 11, is built into the main executable).
pub const OVERLAY_IDS: &[(&str, u32)] = &[
    ("BTL", 1),
    ("STD", 2),
    ("FISH", 3),
    ("EVL", 4),
    ("KAR", 5),
    ("VS", 6),
    ("MOV", 7),
    ("DOO2", 8),
    ("DOOA", 9),
    ("TRN", 10),
    ("DGET", 12),
    ("TRN2", 13),
    ("MURD", 14),
    ("ENDI", 15),
    ("EAB", 16),
];

/// Where the main executable starts (config/us/main.ld's `.header`); symbols at or above it are
/// resident.
const MAIN_BASE: u32 = 0x8009_0000;

/// The object format the game C is built in, from the Rust target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    /// The Mac app.
    MachO,
    /// The Switch (the `horizon` target, linked by devkitA64's ld).
    Elf,
}

impl ObjectFormat {
    pub fn of_target() -> ObjectFormat {
        match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
            Ok("horizon") => ObjectFormat::Elf,
            _ => ObjectFormat::MachO,
        }
    }
}

/// The section an overlay's initialised data goes in (Mach-O section names are at most 16
/// characters; ELF ones must be C identifiers for ld to define `__start_`/`__stop_`); its
/// zero-initialised and uninitialised data go in this name plus `z`.
pub fn section(format: ObjectFormat, unit: &str) -> String {
    match format {
        ObjectFormat::MachO => format!("__dw1_{}", unit.to_lowercase()),
        ObjectFormat::Elf => format!("dw1_{}", unit.to_lowercase()),
    }
}

/// The C files whose `.data` `overlay.ld` places in an overlay (`src/....c.o(.data.*)`).
pub fn overlay_data_files(decomp: &Path) -> BTreeSet<String> {
    let ld = fs::read_to_string(decomp.join("config/us/overlay.ld")).unwrap();
    ld.lines()
        .filter_map(|l| {
            let start = l.find("BUILD_DIR/")? + "BUILD_DIR/".len();
            let rest = &l[start..];
            let end = rest.find(".o(.data.")?;
            Some(rest[..end].to_string())
        })
        .collect()
}

/// Names the main symbol list (config/us/symbols.txt) places in the main executable.
pub fn resident_names(decomp: &Path) -> BTreeSet<String> {
    let text = fs::read_to_string(decomp.join("config/us/symbols.txt")).unwrap();
    text.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once('=')?;
            let hex = rest.trim().strip_prefix("0x")?;
            let addr = u32::from_str_radix(hex.split(';').next()?.trim(), 16).ok()?;
            (addr >= MAIN_BASE).then(|| name.trim().to_string())
        })
        .collect()
}

/// The C tokens of preprocessed source: identifiers, numbers and punctuation (one character
/// each, enough for nesting and `=`, `,`, `;`); string and character literals become one `"`
/// token each.
fn tokens(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'#' {
            // A line marker or pragma left by the preprocessor.
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push("\"".to_string());
        } else if c.is_ascii_alphanumeric() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            out.push(src[start..i].to_string());
        } else {
            out.push((c as char).to_string());
            i += 1;
        }
    }
    out
}

/// The variables a preprocessed C file defines at file scope: (name, has an initialiser).
/// Declarations with `extern` or `typedef`, function declarations and definitions are skipped.
/// Err if a function body declares a `static` (a function-local static, which a wrapper can't
/// name).
pub fn top_level_definitions(pre: &str) -> Result<Vec<(String, bool)>, String> {
    let toks = tokens(pre);
    let mut defs = Vec::new();
    let mut stmt: Vec<&str> = Vec::new();
    // Inside a K&R function header's parameter declarations (`f(a, b) int a; int b; {`).
    let mut kr = false;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        if t == "{" && (kr || (!stmt.contains(&"=") && stmt.last() == Some(&")"))) {
            kr = false;
            // A function body: skip to its end.
            let mut depth = 0;
            loop {
                match toks[i].as_str() {
                    "{" => depth += 1,
                    "}" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    "static" => {
                        return Err(format!(
                            "a function-local static in {}",
                            stmt.iter().take(8).cloned().collect::<Vec<_>>().join(" ")
                        ));
                    }
                    _ => {}
                }
                i += 1;
            }
            stmt.clear();
            i += 1;
            continue;
        }
        if t == ";" && depth_of(&stmt) == 0 {
            if kr || kr_header(&stmt) {
                // A K&R parameter declaration, not a file-scope variable.
                kr = true;
            } else {
                declaration(&stmt, &mut defs);
            }
            stmt.clear();
            i += 1;
            continue;
        }
        stmt.push(t);
        i += 1;
    }
    Ok(defs)
}

/// Whether a statement starts with a K&R function header: a declarator `name (a, b)` whose
/// parameter list is only identifiers, followed by a type (the first parameter declaration).
fn kr_header(toks: &[&str]) -> bool {
    let Some(open) = toks.iter().position(|t| *t == "(") else {
        return false;
    };
    let Some(close) = toks[open..].iter().position(|t| *t == ")").map(|k| k + open) else {
        return false;
    };
    let ident = |t: &str| t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    open > 0
        && ident(toks[open - 1])
        && toks[open + 1..close].iter().all(|t| *t == "," || ident(t))
        && close > open + 1
        && toks.get(close + 1).is_some_and(|t| {
            ident(t) && !matches!(*t, "__attribute__" | "__asm" | "__asm__" | "asm")
        })
}

/// The bracket depth at the end of a token run.
fn depth_of(toks: &[&str]) -> i32 {
    toks.iter()
        .map(|t| match *t {
            "(" | "[" | "{" => 1,
            ")" | "]" | "}" => -1,
            _ => 0,
        })
        .sum()
}

/// Adds the variables one file-scope declaration (`toks`, without its `;`) defines.
fn declaration(toks: &[&str], defs: &mut Vec<(String, bool)>) {
    if toks.is_empty() || toks.contains(&"typedef") || toks.contains(&"extern") {
        return;
    }
    // A struct/union/enum body in the type: skip past it, so its member declarations aren't
    // taken for variables.
    let mut start = 0;
    if let Some(open) = toks.iter().position(|t| *t == "{")
        && !toks[..open].contains(&"=")
    {
        let mut depth = 0;
        for (k, t) in toks.iter().enumerate().skip(open) {
            match *t {
                "{" => depth += 1,
                "}" => {
                    depth -= 1;
                    if depth == 0 {
                        start = k + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    // Declarators split at depth-0 commas.
    let mut parts: Vec<&[&str]> = Vec::new();
    let (mut depth, mut from) = (0, start);
    for (k, t) in toks.iter().enumerate().skip(start) {
        match *t {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            "," if depth == 0 => {
                parts.push(&toks[from..k]);
                from = k + 1;
            }
            _ => {}
        }
    }
    parts.push(&toks[from..]);
    for part in parts {
        let eq = {
            let mut d = 0;
            part.iter().position(|t| {
                match *t {
                    "(" | "[" | "{" => d += 1,
                    ")" | "]" | "}" => d -= 1,
                    _ => {}
                }
                *t == "=" && d == 0
            })
        };
        let decl = &part[..eq.unwrap_or(part.len())];
        if let Some(name) = declarator_name(decl) {
            defs.push((name, eq.is_some()));
        }
    }
}

/// The name a declarator declares, or None for a function declaration (`name(...)` not inside
/// a pointer declarator) or no name.
fn declarator_name(decl: &[&str]) -> Option<String> {
    let ident = |t: &str| {
        t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    // The first `(`: after an identifier it opens a function's parameters (a declaration, not
    // a variable); followed by `*` it's a pointer declarator, a function pointer variable whose
    // name follows the `*` (possibly after `const`).
    // (A parameter list never starts with `*`, so `( *` is checked first.)
    if let Some(p) = decl.iter().position(|t| *t == "(") {
        if decl.get(p + 1) == Some(&"*") {
            let k = (p + 2..decl.len())
                .find(|&k| ident(decl[k]) && !matches!(decl[k], "const" | "volatile"))?;
            // `(*name(...))(...)` declares a function returning a function pointer.
            if decl.get(k + 1) == Some(&"(") {
                return None;
            }
            return Some(decl[k].to_string());
        }
        if p > 0 && ident(decl[p - 1]) {
            return None;
        }
    }
    // Otherwise the last identifier before any `[` dimensions; `name (` is a function.
    let end = decl.iter().position(|t| *t == "[").unwrap_or(decl.len());
    let name = decl[..end].iter().rposition(|t| ident(t))?;
    if decl.get(name + 1) == Some(&"(") {
        return None;
    }
    Some(decl[name].to_string())
}

/// A C identifier for a file's keep list.
fn keep_symbol(unit: &str, rel: &str) -> String {
    let stem: String = Path::new(rel)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("DW1_KEEP_{}_{stem}", unit.to_lowercase())
}

/// The variables of a file a reload must not touch: its resident definitions and its definitions
/// without an initialiser (module docs), from `top_level_definitions`.
pub fn keeps(defs: &[(String, bool)], resident: &BTreeSet<String>) -> Vec<String> {
    let mut keep: BTreeSet<String> = BTreeSet::new();
    for (name, init) in defs {
        if resident.contains(name) || !init {
            keep.insert(name.clone());
        }
    }
    keep.into_iter().collect()
}

/// The wrapper TU for an overlay data file: the file itself (`include` is the path to compile:
/// the original, or its CP932 copy) with its data in the overlay's sections, then the list of
/// variables a reload keeps.
pub fn wrapper(
    format: ObjectFormat,
    unit: &str,
    rel: &str,
    include: &Path,
    keeps: &[String],
) -> String {
    let sec = section(format, unit);
    let (data, bss) = match format {
        ObjectFormat::MachO => (format!("__DATA,{sec}"), format!("__DATA,{sec}z")),
        ObjectFormat::Elf => (sec.clone(), format!("{sec}z")),
    };
    let mut s = format!(
        "/* Generated by crates/decomp-c/build.rs (overlays.rs): {rel}, overlay {unit}. */\n\
         #pragma clang section data=\"{data}\" bss=\"{bss}\"\n\
         #include \"{inc}\"\n\
         #pragma clang section data=\"\" bss=\"\"\n\
         typedef struct {{ void *addr; uintptr_t size; }} dw1_keep_t;\n\
         const dw1_keep_t {sym}[] = {{\n",
        inc = include.display(),
        sym = keep_symbol(unit, rel),
    );
    for k in keeps {
        s.push_str(&format!("\t{{ (void *)&{k}, sizeof({k}) }},\n"));
    }
    s.push_str("\t{ 0, 0 },\n};\n");
    s
}

/// The C side of the table (compiled into the overlay archive): the section bounds, which only
/// C's `__asm` names reach (ld64 synthesises `section$start$SEG$SECT` / `section$end$...`).
pub fn bounds_c(format: ObjectFormat, files: &BTreeMap<String, Vec<String>>) -> String {
    let mut s = String::from("/* Generated by crates/decomp-c/build.rs (overlays.rs). */\n");
    for unit in files.keys() {
        let sec = section(format, unit);
        let u = unit.to_lowercase();
        s.push_str(&match format {
            ObjectFormat::MachO => format!(
                "extern char dw1_{u}_start[] __asm(\"section$start$__DATA${sec}\");\n\
                 extern char dw1_{u}_end[] __asm(\"section$end$__DATA${sec}\");\n\
                 extern char dw1_{u}_zstart[] __asm(\"section$start$__DATA${sec}z\");\n\
                 extern char dw1_{u}_zend[] __asm(\"section$end$__DATA${sec}z\");\n"
            ),
            // A byte in each section makes sure it exists (an overlay without a .bss wouldn't
            // have the second), so ld always defines its `__start_`/`__stop_`.
            ObjectFormat::Elf => format!(
                "#pragma clang section data=\"{sec}\" bss=\"{sec}z\"\n\
                 __attribute__((used)) static char dw1_{u}_anchor = 1;\n\
                 __attribute__((used)) static char dw1_{u}_zanchor;\n\
                 #pragma clang section data=\"\" bss=\"\"\n\
                 extern char dw1_{u}_start[] __asm(\"__start_{sec}\");\n\
                 extern char dw1_{u}_end[] __asm(\"__stop_{sec}\");\n\
                 extern char dw1_{u}_zstart[] __asm(\"__start_{sec}z\");\n\
                 extern char dw1_{u}_zend[] __asm(\"__stop_{sec}z\");\n"
            ),
        });
    }
    s.push_str("int dw1_overlay_bounds(int id, char **start, char **end, char **zstart, char **zend)\n{\n\tswitch (id) {\n");
    for unit in files.keys() {
        let id = OVERLAY_IDS.iter().find(|(u, _)| u == unit).unwrap().1;
        let u = unit.to_lowercase();
        s.push_str(&format!(
            "\tcase {id}:\n\t\t*start = dw1_{u}_start;\n\t\t*end = dw1_{u}_end;\n\t\t*zstart = dw1_{u}_zstart;\n\t\t*zend = dw1_{u}_zend;\n\t\treturn 1;\n"
        ));
    }
    s.push_str("\t}\n\treturn 0;\n}\n");
    s
}

/// The Rust table (`include!`d by src/overlays.rs): per overlay, its number and keep lists (the
/// bounds come from `dw1_overlay_bounds`, `bounds_c`).
pub fn table(files: &BTreeMap<String, Vec<String>>) -> String {
    let mut s = String::from(
        "// Generated by crates/decomp-c/build.rs (overlays.rs).\nunsafe extern \"C\" {\n    fn dw1_overlay_bounds(id: i32, start: *mut *mut u8, end: *mut *mut u8, zstart: *mut *mut u8, zend: *mut *mut u8) -> i32;\n",
    );
    for (unit, rels) in files {
        for rel in rels {
            s.push_str(&format!("    static {}: [Keep; 0];\n", keep_symbol(unit, rel)));
        }
    }
    s.push_str("}\n\n/// Every overlay with data sections: (Overlay number, data start, end, zero start, end, keep lists).\n");
    s.push_str("#[allow(clippy::type_complexity)]\nfn overlay_sections() -> Vec<(u32, *mut u8, *mut u8, *mut u8, *mut u8, Vec<*const Keep>)> {\n    let bounds = |id: i32| {\n        let mut p = [std::ptr::null_mut(); 4];\n        let [a, b, c, d] = &mut p;\n        let ok = unsafe { dw1_overlay_bounds(id, a, b, c, d) };\n        assert_eq!(ok, 1, \"no bounds for overlay {id}\");\n        p\n    };\n    vec![\n");
    for (unit, rels) in files {
        let id = OVERLAY_IDS.iter().find(|(u, _)| u == unit).unwrap().1;
        let keeps: Vec<String> = rels
            .iter()
            .map(|rel| format!("(&raw const {}).cast::<Keep>()", keep_symbol(unit, rel)))
            .collect();
        s.push_str(&format!(
            "        {{ let [a, b, c, d] = bounds({id}); ({id}, a, b, c, d, vec![{}]) }},\n",
            keeps.join(", ")
        ));
    }
    s.push_str("    ]\n}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_definitions_are_found() {
        let src = "typedef struct { int a; int b; } T;\n\
                   extern int e;\n\
                   int x = 1, y, z[3] = {0};\n\
                   static T t = { 1, 2 };\n\
                   void f(T *ot, int aabb);\n\
                   int (*fp)(int) = 0;\n\
                   void (*tbl[2])(void);\n\
                   const char *s = \"a;b{\";\n\
                   struct S { int m; } sv;\n\
                   int g(int q) { int local = 2; return q; }\n\
                   int kr(a, b) int a; char b; { return a; }\n\
                   void noret(void) __attribute__((noreturn));\n\
                   void (*cbset(void (*f)(void)))(void);\n\
                   unsigned char last[2];\n";
        let defs = top_level_definitions(src).unwrap();
        assert_eq!(
            defs,
            [
                ("x", true),
                ("y", false),
                ("z", true),
                ("t", true),
                ("fp", true),
                ("tbl", false),
                ("s", true),
                ("sv", false),
                ("last", false),
            ]
            .map(|(n, i)| (n.to_string(), i))
        );
        assert!(top_level_definitions("int h(void) { static int n; return n; }").is_err());
    }
}
