//! Compiles the game C from the `decomp/` submodule for arm64 macOS, links all of it, and
//! generates Rust bindings for the decomp and SDK structs Rust needs to read.
//!
//! - **Files:** the US build's own lists (`decomp/mk/version/us.mk`): main, SHOP and every
//!   overlay, minus CodeWarrior's startup (`_psstart.c`; the app replaces it).
//! - **Headers:** `include/` here first (the forced `dw1_native.h` and native replacements for
//!   PS1-only headers), then the decomp's, then Psy-Q's (`external/psyq_headers`), then the
//!   generated text headers.
//! - **Text:** the decomp keeps game text in `text/**/*.yaml`; its own generator
//!   (`tools/gen_text.py`) writes the headers into `OUT_DIR`, as Shift-JIS byte escapes.
//! - **Shift-JIS literals:** the PS1 build compiles with `-fexec-charset=CP932`, which clang
//!   lacks, and clang would silently store UTF-8. Files with non-ASCII string or character
//!   literals are copied into `OUT_DIR` with those characters as CP932 octal escapes.
//! - **Linking:** the whole archive is linked (`+whole-archive`), so every symbol any game file needs must
//!   resolve (in `psyq`, or here), and a build check fails if the game C would bind to a
//!   macOS libc function instead of the PS1 one.

#[path = "overlays.rs"]
mod overlays;

use overlays::ObjectFormat;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "src/literals.rs"]
mod literals;

/// Units of the US build that this crate compiles, by their `us.mk` variable prefix.
const UNITS: &[&str] = &[
    "MAIN", "SHOP", "BTL", "DGET", "DOO2", "DOOA", "EAB", "ENDI", "EVL", "FISH", "KAR", "MOV",
    "MURD", "STD", "TRN2", "TRN", "VS",
];

/// CodeWarrior's startup code (copyrighted, MIPS assembly); the native app starts the game.
const EXCLUDED: &[&str] = &["src/main/_psstart.c"];

/// Host symbols the game C may reference: `setjmp.h`'s replacement, clang's stack protector,
/// and the memory functions clang emits itself for struct copies and zeroing (source calls
/// are renamed to `dw1_` by `dw1_native.h`, so only compiler-generated ones remain, with the
/// host ABI). Anything else macOS's libSystem exports is a mistake: a PS1 libc call that
/// `dw1_native.h` doesn't rename.
const HOST_ALLOWED: &[&str] = &[
    "__setjmp",
    "__longjmp",
    "___stack_chk_fail",
    "___stack_chk_guard",
    "_memcpy",
    "_memmove",
    "_memset",
    "_bzero",
];

/// The newlib symbols the game C may reference on the Switch: `setjmp.h`'s replacement and the
/// memory functions clang emits itself (the `HOST_ALLOWED` of the ELF build).
const NEWLIB_ALLOWED: &[&str] = &["setjmp", "longjmp", "memcpy", "memmove", "memset"];

const SWITCH_TARGET_ARGS: &[&str] = &["--target=aarch64-none-elf", "-ffreestanding"];

/// Headers bindgen reads, relative to the include path.
const BOUND_HEADERS: &[&str] = &[
    "sys/types.h",
    "libgte.h",
    "libgpu.h",
    "libgs.h",
    "libsnd.h",
    "libspu.h",
    "libcd.h",
    "kernel.h",
    "dw/params.h",
    "dw/model.h",
    "dw/entity.h",
    "dw/script.h",
    "dw/item.h",
    "dw/combat.h",
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // The save-card test needs Ember's fixture (local, gitignored); without it the test is ignored.
    println!("cargo::rustc-check-cfg=cfg(dw1_fixture_card)");
    let fixture = manifest.join("../../captures/fixtures/ember-day2-gabumon.mcd");
    println!("cargo:rerun-if-changed={}", fixture.display());
    if fixture.exists() {
        println!("cargo:rustc-cfg=dw1_fixture_card");
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let decomp = manifest
        .join("../../decomp")
        .canonicalize()
        .expect("decomp/ submodule missing: run `git submodule update --init decomp`");
    let workspace = manifest.join("../..").canonicalize().unwrap();

    let native_include = manifest.join("include");
    let psyq_include = decomp.join("external/psyq_headers/mw_lib41/include");
    assert!(
        psyq_include.join("libgs.h").exists(),
        "Psy-Q headers missing: run `git -C decomp submodule update --init external/psyq_headers`"
    );
    let python = python(&workspace);
    let generated = out.join("generated");
    generate_text_headers(&decomp, &python, &generated);

    let format = ObjectFormat::of_target();
    let mut cpp_args = preprocessor_args(&[
        &native_include,
        &decomp.join("include"),
        &psyq_include,
        &generated,
    ]);
    if format == ObjectFormat::Elf {
        cpp_args.extend(SWITCH_TARGET_ARGS.iter().map(|a| a.to_string()));
    }
    let sources = c_sources(&decomp);
    let objects_dir = out.join("sjis");

    let mut build = cc::Build::new();
    if format == ObjectFormat::Elf {
        configure_switch(&mut build);
    }
    build
        .std("gnu99")
        .warnings(true)
        .extra_warnings(true)
        // Whole archive: every game file is linked, so every unresolved call shows up now.
        .link_lib_modifier("+whole-archive")
        // `DW1_WARNINGS=1 cargo build -p decomp-c` prints every warning (Phase 1 triage).
        .cargo_warnings(env::var_os("DW1_WARNINGS").is_some());
    for arg in &cpp_args {
        build.flag(arg);
    }
    // From the decomp's own GCC flags (decomp/mk/toolchain/gcc.mk): the matching C relies on
    // wrapping and loose aliasing, and must not have libc calls swapped for builtins.
    build
        .flag("-fwrapv")
        .flag("-fno-strict-aliasing")
        .flag("-fno-common")
        .flag("-fno-builtin")
        // Plain char is signed for MIPS GCC and for Apple arm64; pinned so it can't drift.
        // The retail CodeWarrior default is unconfirmed (docs/survey.md, item 8).
        .flag("-fsigned-char")
        // Every game function keeps its own frame, so the replay host's caller check (the
        // function that called `PadRead`, crates/psyq/src/libetc.rs) names the same function at
        // any optimisation level: no inlining, no sibling calls.
        .flag("-fno-inline")
        .flag("-fno-optimize-sibling-calls")
        // Noise from matching-decomp style, not ABI hazards (docs/phase1-report.md).
        .flag("-Wno-unknown-pragmas")
        .flag("-Wno-unused-parameter");
    // `DW1_SANITIZE=undefined` (or `address`, comma lists as clang takes them) compiles the game
    // C with clang's sanitizers, to find PS1-isms that don't crash at once; link the runtime with
    // `RUSTFLAGS="-C link-arg=-fsanitize=<same>"` and a separate `CARGO_TARGET_DIR`. Off by
    // default; reports go to stderr and the run continues.
    println!("cargo:rerun-if-env-changed=DW1_SANITIZE");
    if let Some(list) = env::var("DW1_SANITIZE").ok().filter(|s| !s.is_empty()) {
        build
            .flag(format!("-fsanitize={list}"))
            // report and go on (ASan stops at the first error otherwise; with
            // ASAN_OPTIONS=halt_on_error=0)
            .flag("-fsanitize-recover=all")
            .flag("-fno-omit-frame-pointer");
    }
    // Overlay files whose initialised data the PS1 reloads with the overlay are compiled
    // through wrappers into a second archive (overlays.rs).
    let overlay_build_base = build.clone();
    let mut overlay_build = build.clone();
    let overlay_data = overlays::overlay_data_files(&decomp);
    let resident = overlays::resident_names(&decomp);
    let wrappers_dir = out.join("overlays");
    let mut overlay_files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let texts: Vec<(PathBuf, String)> = sources
        .iter()
        .map(|(_, rel)| {
            let path = decomp.join(rel);
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
            (path, text)
        })
        .collect();
    let cp932 = cp932_table(
        &python,
        texts
            .iter()
            .flat_map(|(_, text)| literals::literal_chars(text)),
    );
    for ((path, text), (unit, rel)) in texts.iter().zip(&sources) {
        let file = match literals::encode_literals(text, rel, &mut |c| cp932.get(&c).cloned()) {
            None => path.clone(),
            Some(encoded) => {
                let copy = objects_dir.join(rel);
                fs::create_dir_all(copy.parent().unwrap()).unwrap();
                // Diagnostics and debug info keep pointing at the original file.
                let body = format!("#line 1 \"{}\"\n{encoded}", path.display());
                write_if_changed(&copy, &body);
                copy
            }
        };
        let is_overlay = overlays::OVERLAY_IDS.iter().any(|(u, _)| u == unit);
        if is_overlay && overlay_data.contains(rel) {
            // The file's file-scope definitions, from its preprocessed text (so definitions
            // built by macros count).
            let pre = Command::new(overlay_build_base.get_compiler().path())
                .args(["-E", "-P", "-std=gnu99", "-fsigned-char"])
                .args(&cpp_args)
                .arg(&file)
                .output()
                .unwrap_or_else(|e| panic!("preprocessing {rel}: {e}"));
            assert!(
                pre.status.success(),
                "preprocessing {rel}: {}",
                String::from_utf8_lossy(&pre.stderr)
            );
            let defs = overlays::top_level_definitions(&String::from_utf8_lossy(&pre.stdout))
                .unwrap_or_else(|e| panic!("{rel}: {e}; overlays.rs can't keep it"));
            let keeps = overlays::keeps(&defs, &resident);
            let wrapper = wrappers_dir.join(rel);
            fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
            write_if_changed(&wrapper, &overlays::wrapper(format, unit, rel, &file, &keeps));
            overlay_build.file(&wrapper);
            overlay_files.entry(unit.to_string()).or_default().push(rel.clone());
        } else {
            build.file(&file);
        }
    }
    let bounds = out.join("overlay_bounds.c");
    write_if_changed(&bounds, &overlays::bounds_c(format, &overlay_files));
    overlay_build.file(&bounds);
    build.compile("decomp");
    overlay_build.compile("decomp_overlays");
    write_if_changed(&out.join("overlays_table.rs"), &overlays::table(&overlay_files));

    for archive in ["libdecomp.a", "libdecomp_overlays.a"] {
        match format {
            ObjectFormat::MachO => check_host_symbols(&out.join(archive)),
            ObjectFormat::Elf => check_newlib_symbols(&out.join(archive)),
        }
    }

    generate_bindings(&cpp_args, &out);

    // Directories are watched recursively, so any header, C or text edit rebuilds.
    for dir in [
        decomp.join("src"),
        decomp.join("include"),
        decomp.join("text"),
        decomp.join("tools/gen_text.py"),
        decomp.join("mk/version/us.mk"),
        decomp.join("config/us/overlay.ld"),
        decomp.join("config/us/symbols.txt"),
        psyq_include,
        native_include,
    ] {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=overlays.rs");
    println!("cargo:rerun-if-changed=src/literals.rs");
    println!("cargo:rerun-if-env-changed=DW1_WARNINGS");
    println!("cargo:rerun-if-env-changed=DW1_PYTHON");
}

/// Preprocessor setup shared by the C compiler and bindgen, so the bindings describe the C
/// exactly as it is compiled (including everything `dw1_native.h` changes).
fn preprocessor_args(include_dirs: &[&Path]) -> Vec<String> {
    let mut args: Vec<String> = include_dirs
        .iter()
        .map(|dir| format!("-I{}", dir.display()))
        .collect();
    args.extend([
        "-DVERSION_US".into(),
        "-DLANGUAGE_C".into(),
        "-include".into(),
        include_dirs[0].join("dw1_native.h").display().to_string(),
    ]);
    args
}

/// The C files of every unit in `UNITS`, from the `<UNIT>_C_SRC := \` lists in `us.mk`, with
/// their unit.
fn c_sources(decomp: &Path) -> Vec<(&'static str, String)> {
    let mk = fs::read_to_string(decomp.join("mk/version/us.mk")).unwrap();
    let mut sources = Vec::new();
    for unit in UNITS {
        let header = format!("{unit}_C_SRC :=");
        let mut lines = mk.lines().skip_while(|l| !l.starts_with(&header));
        assert!(lines.next().is_some(), "us.mk has no {header}");
        let before = sources.len();
        for line in lines {
            let file = line.trim().trim_end_matches('\\').trim();
            if file.is_empty() {
                break;
            }
            if !EXCLUDED.contains(&file) {
                sources.push((*unit, file.to_string()));
            }
        }
        assert!(sources.len() > before, "us.mk lists no files for {unit}");
    }
    sources
}

/// Runs the decomp's text generator for every `text/<dir>/<name>.yaml`, writing
/// `<generated>/text/<dir>/<name>.h` for the US release.
fn generate_text_headers(decomp: &Path, python: &Path, generated: &Path) {
    let text = decomp.join("text");
    for dir in fs::read_dir(&text).unwrap() {
        let dir = dir.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for yaml in fs::read_dir(&dir).unwrap() {
            let yaml = yaml.unwrap().path();
            if yaml.extension().is_none_or(|e| e != "yaml") {
                continue;
            }
            let rel = yaml.strip_prefix(&text).unwrap().with_extension("h");
            let header = generated.join("text").join(rel);
            fs::create_dir_all(header.parent().unwrap()).unwrap();
            let status = Command::new(python)
                .arg(decomp.join("tools/gen_text.py"))
                .args(["us".as_ref(), yaml.as_os_str(), header.as_os_str()])
                .status()
                .unwrap_or_else(|e| panic!("running {}: {e} (set DW1_PYTHON)", python.display()));
            assert!(
                status.success(),
                "gen_text.py failed on {} (needs PyYAML: `pip install -r decomp/requirements.txt`)",
                yaml.display()
            );
        }
    }
}

/// The Python that runs the decomp's tools: `DW1_PYTHON`, else the workspace's `.venv`, else
/// `python3` (docs/toolchain.md).
fn python(workspace: &Path) -> PathBuf {
    env::var_os("DW1_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let venv = workspace.join(".venv/bin/python");
            if venv.exists() {
                venv
            } else {
                "python3".into()
            }
        })
}

/// CP932 bytes for each of `chars`, from Python's `cp932` codec: the one the decomp's text
/// generator uses, so literals in C and text from YAML agree. (Rust's Shift-JIS tables are
/// WHATWG's, which differ from Microsoft's CP932 for about 2,300 characters.) A character
/// Python can't encode is left out, and the encoder then names it.
fn cp932_table(python: &Path, chars: impl Iterator<Item = char>) -> BTreeMap<char, Vec<u8>> {
    let chars: BTreeSet<char> = chars.collect();
    if chars.is_empty() {
        return BTreeMap::new();
    }
    const SCRIPT: &str = "import sys\n\
        for c in sys.stdin.read():\n\
        \ttry: print(c.encode('cp932').hex())\n\
        \texcept UnicodeEncodeError: print('-')\n";
    let mut child = Command::new(python)
        .args(["-c", SCRIPT])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .spawn()
        .unwrap_or_else(|e| panic!("running {}: {e} (set DW1_PYTHON)", python.display()));
    let input: String = chars.iter().collect();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "Python's cp932 encoder failed");
    let lines: Vec<&str> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(
        lines.len(),
        chars.len(),
        "Python's cp932 encoder lost characters"
    );
    chars
        .into_iter()
        .zip(lines)
        .filter(|(_, hex)| *hex != "-")
        .map(|(c, hex)| {
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            (c, bytes)
        })
        .collect()
}

/// Writes `path` only when its contents change, so unchanged copies don't recompile.
fn write_if_changed(path: &Path, contents: &str) {
    if fs::read_to_string(path).is_ok_and(|old| old == contents) {
        return;
    }
    fs::write(path, contents).unwrap();
}

/// Fails the build if the archive needs a symbol macOS's libSystem would provide, other than
/// `HOST_ALLOWED`: that would be a PS1 libc call silently bound to the host's version.
fn check_host_symbols(archive: &Path) {
    let nm = |args: &[&str]| -> BTreeSet<String> {
        let output = Command::new("nm")
            .args(args)
            .arg(archive)
            .output()
            .expect("running nm");
        assert!(
            output.status.success(),
            "nm failed on {}",
            archive.display()
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty() && !l.ends_with(':'))
            .map(str::to_string)
            .collect()
    };
    let undefined = nm(&["-u", "-j"]);
    let defined = nm(&["-g", "-j", "-U"]);

    let sdk = Command::new("xcrun")
        .arg("--show-sdk-path")
        .output()
        .expect("running xcrun");
    let sdk = PathBuf::from(String::from_utf8(sdk.stdout).unwrap().trim());
    let mut host = BTreeSet::new();
    for tbd in fs::read_dir(sdk.join("usr/lib/system")).unwrap() {
        let tbd = tbd.unwrap().path();
        if tbd.extension().is_some_and(|e| e == "tbd") {
            let text = fs::read_to_string(&tbd).unwrap();
            host.extend(
                text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                    .filter(|s| s.starts_with('_'))
                    .map(str::to_string),
            );
        }
    }
    let bound_to_host: Vec<_> = undefined
        .iter()
        .filter(|s| {
            !defined.contains(*s) && host.contains(*s) && !HOST_ALLOWED.contains(&s.as_str())
        })
        .collect();
    assert!(
        bound_to_host.is_empty(),
        "game C would call macOS libc for {bound_to_host:?}: rename them in \
         include/dw1_native.h and implement them in crates/psyq"
    );
}

fn devkitpro() -> PathBuf {
    println!("cargo:rerun-if-env-changed=DEVKITPRO");
    env::var_os("DEVKITPRO").map_or_else(|| PathBuf::from("/opt/devkitpro"), PathBuf::from)
}

fn devkita64_tool(name: &str) -> PathBuf {
    devkitpro().join("devkitA64/bin").join(format!("aarch64-none-elf-{name}"))
}

/// The Switch build: clang makes the ELF objects and devkitA64's `ar` archives them. The code is
/// PIE for the `.nro` and keeps its frame pointers, which `switch/source/backtrace.c` walks.
fn configure_switch(build: &mut cc::Build) {
    let opt = env::var("OPT_LEVEL").unwrap_or_else(|_| "2".into());
    build
        .compiler("clang")
        .archiver(devkita64_tool("ar"))
        .no_default_flags(true)
        .flag("-march=armv8-a+crc+crypto")
        .flag("-mtune=cortex-a57")
        .flag("-fPIE")
        .flag("-ffunction-sections")
        .flag("-fno-omit-frame-pointer")
        .flag("-mno-omit-leaf-frame-pointer")
        .flag(format!("-O{opt}"))
        .flag("-g");
}

fn elf_symbols(archive: &Path, defined: bool) -> BTreeSet<String> {
    let mut nm = Command::new(devkita64_tool("nm"));
    nm.arg("--format=just-symbols");
    if defined {
        nm.args(["--extern-only", "--defined-only"]);
    } else {
        nm.arg("--undefined-only");
    }
    let output = nm.arg(archive).output().expect("running devkitA64's nm");
    assert!(output.status.success(), "nm failed on {}", archive.display());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty() && !l.ends_with(':'))
        .map(str::to_string)
        .collect()
}

/// `check_host_symbols` for the Switch: no game C call may bind to devkitA64's newlib.
fn check_newlib_symbols(archive: &Path) {
    let lib = devkitpro().join("devkitA64/aarch64-none-elf/lib");
    let mut host = BTreeSet::new();
    for name in ["libc.a", "libm.a", "libsysbase.a"] {
        let pic = lib.join("pic").join(name);
        let path = if pic.exists() { pic } else { lib.join(name) };
        if path.exists() {
            host.extend(elf_symbols(&path, true));
        }
    }
    assert!(!host.is_empty(), "newlib not found in {}", lib.display());
    let defined = elf_symbols(archive, true);
    let bound_to_host: Vec<_> = elf_symbols(archive, false)
        .into_iter()
        .filter(|s| !defined.contains(s) && host.contains(s) && !NEWLIB_ALLOWED.contains(&s.as_str()))
        .collect();
    assert!(
        bound_to_host.is_empty(),
        "game C would call newlib for {bound_to_host:?}: rename them in include/dw1_native.h and \
         implement them in crates/psyq"
    );
}

fn generate_bindings(cpp_args: &[String], out: &Path) {
    let mut bindings = bindgen::Builder::default()
        .clang_args(["-std=gnu99", "-fsigned-char"])
        .clang_args(cpp_args)
        .allowlist_type("DigimonPara")
        .allowlist_type("PartnerPara")
        .allowlist_type("TamerEntity")
        .allowlist_type("PartnerEntity")
        .allowlist_type("CombatData")
        .allowlist_type("PositionData")
        .allowlist_type("ScriptState")
        .allowlist_type("Inventory")
        .allowlist_type("MATRIX")
        .allowlist_type("TIM_IMAGE")
        .allowlist_type("VECTOR")
        .allowlist_type("SVECTOR")
        .allowlist_type("_GsFCALL")
        .allowlist_type("RECT")
        .allowlist_type("DRAWENV")
        .allowlist_type("DISPENV")
        .allowlist_type("DR_OFFSET")
        .allowlist_type("GsOT")
        .allowlist_type("GsDOBJ2")
        .allowlist_type("GsCOORDINATE2")
        .allowlist_type("GsIMAGE")
        .allowlist_type("GsRVIEW2")
        .allowlist_type("GsVIEW2")
        .allowlist_type("GsF_LIGHT")
        .allowlist_type("TMD_STRUCT")
        .allowlist_type("TMDModel")
        .allowlist_type("VabHdr")
        .allowlist_type("SpuReverbAttr")
        .allowlist_type("SpuVolume")
        .allowlist_type("CdlLOC")
        .allowlist_type("CdlFILE")
        .allowlist_type("DIRENTRY")
        .use_core()
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for header in BOUND_HEADERS {
        bindings = bindings.header_contents(
            &format!("bound_{}.h", header.replace(['/', '.'], "_")),
            &format!("#include <{header}>\n"),
        );
    }
    bindings
        .generate()
        .expect("bindgen failed on the decomp headers")
        .write_to_file(out.join("bindings.rs"))
        .unwrap();
}
