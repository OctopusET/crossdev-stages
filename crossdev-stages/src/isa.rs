//! What a board's `-march` promises, against what its binaries actually use.
//!
//! Portage decides a binary package is usable by matching CHOST, KEYWORDS, USE
//! and CPU_FLAGS_X86.  It never matches CFLAGS.  So a package built for one
//! board installs into another that cannot run it, and nothing says so until
//! the hardware hits the first illegal instruction -- which is how the rv32
//! board died on cached `-march=rv32imac` binaries whose compressed opcodes
//! its SoC does not implement.
//!
//! There are two ways to ask, and they are not interchangeable.
//!
//! [`Method::Flags`] takes the CFLAGS portage recorded for each installed
//! package and asks the board's own gcc what ISA those flags produce.  Package
//! granularity, every architecture, and the only measurement here that can
//! fail a build.
//!
//! [`Method::Attributes`] reads `Tag_RISCV_arch` out of each ELF file.  File
//! granularity and it needs no package database, but RISC-V is the only
//! architecture that records this -- x86 and arm keep their ISA nowhere in the
//! file -- and it is advisory only.  A library that dispatches on `getauxval`
//! compiles its vector path as a separate object with its own `-march`, so the
//! attribute names extensions the CPU is never asked to execute: libgcrypt and
//! leancrypto in the riscv64 target do exactly that.  Recorded CFLAGS cannot
//! make that mistake, because what portage recorded for both is `-O2 -pipe`.

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8Path;

use crate::board::BoardConfig;
use crate::container::SandboxRunner;
use crate::error::Result;

/// How a set of extensions was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The flags portage recorded, put back through the board's compiler.
    Flags,
    /// The ISA the ELF file itself carries.
    Attributes,
}

impl Method {
    fn label(self) -> &'static str {
        match self {
            Method::Flags => "flags",
            Method::Attributes => "attributes",
        }
    }
}

/// One distinct ISA found by one method, and what it costs.
#[derive(Debug)]
pub struct Finding {
    /// Extensions these use that the board's `-march` does not have.
    pub extra: BTreeSet<String>,
    /// Extensions the board has that these were not built for.
    pub missing: BTreeSet<String>,
    pub count: usize,
    /// A few of them, enough to go and look: `cat/pkg` for flags, paths for
    /// attributes.
    pub examples: Vec<String>,
}

/// What the recorded CFLAGS say, per package.
#[derive(Debug)]
pub struct FlagsReport {
    pub expected: BTreeSet<String>,
    /// The board's own `-m` flags, which is what the header can print: the
    /// expanded set is 68 macros wide on x86 and says nothing a reader wants.
    pub machine_flags: String,
    /// Packages whose CFLAGS were read.
    pub scanned: usize,
    /// Packages whose flags this toolchain refuses to answer for.  Neither a
    /// pass nor a fault.
    pub unresolved: Vec<String>,
    pub findings: Vec<Finding>,
}

/// What the ELF files say, per file.
#[derive(Debug)]
pub struct AttrReport {
    pub expected: BTreeSet<String>,
    pub scanned: usize,
    pub findings: Vec<Finding>,
}

#[derive(Debug)]
pub struct Report {
    /// `None` when the image carries no package database to read.
    pub flags: Option<FlagsReport>,
    /// `None` when this architecture's ELF files record no ISA.
    pub attrs: Option<AttrReport>,
    pub arch: String,
}

/// Whether this architecture writes its ISA into the ELF file.
fn attrs_apply(arch: &str) -> bool {
    arch.starts_with("riscv")
}

/// `cat/pkg<TAB>CFLAGS` for every package portage recorded installing.
///
/// One awk over the glob rather than a shell loop: a target holds hundreds of
/// these and only the first line of each file is the flags.
const RECORDED_FLAGS: &str = r#"
awk 'FNR == 1 { n = split(FILENAME, p, "/"); printf "%s/%s\t%s\n", p[n-2], p[n-1], $0 }' \
    %ROOT%/var/db/pkg/*/*/CFLAGS 2>/dev/null
true
"#;

/// Which of these packages installed at least one ELF file.
///
/// A wrong `-march` recorded against a package that ships no code cannot fault
/// anything: `acct-group/ntp` and `dev-perl/*` carry the same recorded flags as
/// curl and install nothing that executes.  Asking the installed path for its
/// magic also answers through the `/sbin -> usr/bin` symlinks a merged-usr tree
/// has, which a string rule over the recorded paths gets wrong.
const OWNS_ELF: &str = r#"
for p in %PKGS%; do
    awk '$1 == "obj" { print $2 }' %ROOT%/var/db/pkg/"$p"/CONTENTS 2>/dev/null \
    | while read -r f; do
        if [ "$(head -c 4 "%ROOT%$f" 2>/dev/null)" = "$(printf '\177ELF')" ]; then
            printf '%s\n' "$p"
            break
        fi
      done
done
true
"#;

/// The ISA a set of machine flags produces, asked of the board's own toolchain
/// rather than worked out from a table.
///
/// `-march=rva23u64` and `-march=x86-64-v3` both name far more extensions than
/// they spell, and which ones depends on the GCC version.  Asking the compiler
/// gives the exact set it uses everywhere else, with no expansion rules to keep
/// in step with the toolchain.  The board's flags and a package's recorded
/// flags go through this same function, so the two sides are always in the same
/// vocabulary.
fn probe(runner: &SandboxRunner, board: &BoardConfig, flags: &str) -> Result<BTreeSet<String>> {
    let cc = &board.cross_compile;
    if attrs_apply(&board.arch) {
        let out = runner.run_output(&format!(
            "set -e
             {cc}gcc {flags} -c -o /tmp/isa-probe.o -x c /dev/null
             {cc}readelf -A /tmp/isa-probe.o"
        ))?;
        Ok(parse_attributes(&out).unwrap_or_default())
    } else {
        // No attribute to read, so ask the preprocessor: every feature an
        // `-march` turns on has a predefined macro, and that is the same
        // question the code being compiled asks.
        let out = runner.run_output(&format!("{cc}gcc {flags} -dM -E -x c /dev/null"))?;
        Ok(parse_macros(&out))
    }
}

/// Read `Tag_RISCV_arch` out of every ELF file under `root`.
///
/// One shell loop rather than one container per file: a rootfs holds thousands
/// of these.
fn scan(runner: &SandboxRunner, root: &str, cross: &str) -> Result<Vec<(String, String)>> {
    let out = runner.run_output(&format!(
        "find {root} -type f \\( -perm -u+x -o -name '*.so' -o -name '*.so.*' \\) -print0 \\
         | xargs -0 -r -n 1 sh -c '
             a=$({cross}readelf -A \"$0\" 2>/dev/null | \\
                 sed -n \"s/.*Tag_RISCV_arch: \\\"\\(.*\\)\\\"/\\1/p\" | head -n 1)
             [ -n \"$a\" ] && printf \"%s\\t%s\\n\" \"$0\" \"$a\"
             true'"
    ))?;
    Ok(parse_records(&out))
}

/// Compare everything under `root` against what the board's toolchain emits.
pub fn verify(runner: &SandboxRunner, board: &BoardConfig, root: &str) -> Result<Report> {
    let machine = machine_flags(&board.effective_cflags());
    let expected = probe(runner, board, &machine)?;
    let attrs = if attrs_apply(&board.arch) {
        Some(attr_report(runner, board, root, &expected)?)
    } else {
        None
    };
    Ok(Report {
        flags: flags_report(runner, board, root, machine, &expected)?,
        attrs,
        arch: board.arch.clone(),
    })
}

/// The recorded-CFLAGS half.  `None` when there is no package database, which
/// is every rootfs portage did not build.
fn flags_report(
    runner: &SandboxRunner,
    board: &BoardConfig,
    root: &str,
    board_flags: String,
    expected: &BTreeSet<String>,
) -> Result<Option<FlagsReport>> {
    let records = parse_records(&runner.run_output(&RECORDED_FLAGS.replace("%ROOT%", root))?);
    if records.is_empty() {
        return Ok(None);
    }

    // Group by the machine flags: hundreds of packages, a handful of distinct
    // answers, and one probe per answer.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (pkg, cflags) in &records {
        groups
            .entry(machine_flags(cflags))
            .or_default()
            .push(pkg.clone());
    }

    let mut unresolved = Vec::new();
    let mut answers: Answers = BTreeMap::new();
    for (flags, pkgs) in groups {
        let Ok(observed) = probe(runner, board, &flags) else {
            unresolved.extend(pkgs);
            continue;
        };
        answer(&mut answers, expected, &observed, pkgs);
    }

    let mut findings = Vec::new();
    for ((extra, missing), pkgs) in answers {
        // Only a package that shipped code can fault, and only the extra side
        // faults, so that is the only side worth the scan.
        let pkgs = if extra.is_empty() {
            pkgs
        } else {
            owns_elf(runner, root, &pkgs)?
        };
        if pkgs.is_empty() {
            continue;
        }
        findings.push(finding(extra, missing, pkgs));
    }
    sort_findings(&mut findings);
    unresolved.sort();

    Ok(Some(FlagsReport {
        expected: expected.clone(),
        machine_flags: board_flags,
        scanned: records.len(),
        unresolved,
        findings,
    }))
}

/// The ELF-attribute half.  RISC-V only, and never fatal.
fn attr_report(
    runner: &SandboxRunner,
    board: &BoardConfig,
    root: &str,
    expected: &BTreeSet<String>,
) -> Result<AttrReport> {
    let found = scan(runner, root, &board.cross_compile)?;

    // Group by ISA string: a rootfs has thousands of files and a handful of
    // distinct answers, and the answer is what matters.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, arch) in &found {
        groups.entry(arch.clone()).or_default().push(path.clone());
    }

    let mut answers: Answers = BTreeMap::new();
    for (arch, paths) in groups {
        answer(&mut answers, expected, &parse_arch(&arch), paths);
    }
    let mut findings: Vec<Finding> = answers
        .into_iter()
        .map(|((extra, missing), paths)| finding(extra, missing, paths))
        .collect();
    sort_findings(&mut findings);

    Ok(AttrReport {
        expected: expected.clone(),
        scanned: found.len(),
        findings,
    })
}

/// One (extra, missing) answer, and everything that gave it.
type Answers = BTreeMap<(BTreeSet<String>, BTreeSet<String>), Vec<String>>;

/// File one measurement under the answer it gives.
///
/// Keyed on the difference rather than on what was measured: glibc records
/// `-mbranch-protection=none` and nothing else does, which is a separate probe
/// and the same answer, and two lines saying it is one line too many.
fn answer(
    answers: &mut Answers,
    expected: &BTreeSet<String>,
    observed: &BTreeSet<String>,
    names: Vec<String>,
) {
    let extra: BTreeSet<String> = observed.difference(expected).cloned().collect();
    let missing: BTreeSet<String> = expected.difference(observed).cloned().collect();
    if extra.is_empty() && missing.is_empty() {
        return;
    }
    answers.entry((extra, missing)).or_default().extend(names);
}

fn finding(extra: BTreeSet<String>, missing: BTreeSet<String>, names: Vec<String>) -> Finding {
    Finding {
        extra,
        missing,
        count: names.len(),
        examples: names.into_iter().take(3).collect(),
    }
}

/// Ask the image which of `pkgs` installed something that executes.
fn owns_elf(runner: &SandboxRunner, root: &str, pkgs: &[String]) -> Result<Vec<String>> {
    let out = runner.run_output(
        &OWNS_ELF
            .replace("%ROOT%", root)
            .replace("%PKGS%", &pkgs.join(" ")),
    )?;
    let owners: BTreeSet<&str> = out.lines().map(str::trim).collect();
    Ok(pkgs
        .iter()
        .filter(|p| owners.contains(p.as_str()))
        .cloned()
        .collect())
}

/// Loudest first: the ones that fault, then the biggest.
fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        b.extra
            .is_empty()
            .cmp(&a.extra.is_empty())
            .then(b.count.cmp(&a.count))
    });
}

/// Split `key<TAB>value` lines, dropping anything that is neither.
fn parse_records(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('\t')?;
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// The `-m` tokens of a CFLAGS string, which are the only ones that decide an
/// ISA.
///
/// Filtering the probe's input rather than its output: `-O0` moves
/// `__OPTIMIZE__` and `-fpic` moves `__PIC__`, and neither is a machine
/// property, so the tokens that produce them must not reach the probe at all.
fn machine_flags(cflags: &str) -> String {
    cflags
        .split_whitespace()
        .filter(|t| t.starts_with("-m"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pull the arch string out of `readelf -A` output.
fn parse_attributes(text: &str) -> Option<BTreeSet<String>> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("Tag_RISCV_arch: "))
        .map(|v| parse_arch(v.trim().trim_matches('"')))
}

/// Pull the feature macros out of `gcc -dM -E` output.
///
/// A feature macro is spelled in capitals and defined to 1 -- `__SSE2__`,
/// `__ARM_NEON`, `__ARM_FEATURE_FMA`.  Everything else the preprocessor dumps
/// is either lower case (`__linux__`, `__riscv_v_intrinsic`) or carries a value
/// that is not a flag (`__CHAR_BIT__ 8`), and neither shape is a CPU feature.
fn parse_macros(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let ("#define", Some(name), Some("1"), None) =
                (f.next()?, f.next(), f.next(), f.next())
            else {
                return None;
            };
            name.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                .then(|| name.to_string())
        })
        .collect()
}

/// `rv64i2p1_m2p0_zicsr2p0` -> {i, m, zicsr}.
///
/// The XLEN prefix is dropped: a board and its binaries cannot disagree about
/// it without failing far more loudly than this check.
fn parse_arch(arch: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, token) in arch.split('_').enumerate() {
        let token = if i == 0 {
            token
                .strip_prefix("rv64")
                .or_else(|| token.strip_prefix("rv32"))
                .or_else(|| token.strip_prefix("rv128"))
                .unwrap_or(token)
        } else {
            token
        };
        let name = strip_version(token);
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
    out
}

/// Drop the `<major>p<minor>` an ELF attribute carries and a `-march=` does
/// not: `zicsr2p0` -> `zicsr`, `zvl256b` -> `zvl256b`.
fn strip_version(token: &str) -> &str {
    let bytes = token.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == bytes.len() || i == 0 || bytes[i - 1] != b'p' {
        return token;
    }
    i -= 1;
    let after_p = i;
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == after_p {
        return token;
    }
    &token[..i]
}

fn join(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(" ")
}

/// Print one method's findings, and say whether any of them is a fault.
fn print_findings(findings: &[Finding], unit: &str) -> bool {
    let mut fault = false;
    for f in findings {
        if f.extra.is_empty() {
            println!(
                "    {} {unit} built without {} (slower, but they run)",
                f.count,
                join(&f.missing)
            );
        } else {
            fault = true;
            println!(
                "    {} {unit} use {}, which this board does not have",
                f.count,
                join(&f.extra)
            );
        }
        for example in &f.examples {
            println!("        {example}");
        }
    }
    fault
}

/// Print a report, and say whether anything in it is fatal.
pub fn print(report: &Report, root: &Utf8Path) -> bool {
    let mut fatal = false;
    match &report.flags {
        Some(r) => {
            println!(
                "ISA check ({}): {} packages under {root}, board built with {}",
                Method::Flags.label(),
                r.scanned,
                if r.machine_flags.is_empty() {
                    "no -m flags of its own"
                } else {
                    &r.machine_flags
                }
            );
            if r.findings.is_empty() {
                println!("    every package matches the board");
            } else {
                fatal = print_findings(&r.findings, "packages");
            }
            if !r.unresolved.is_empty() {
                // Not a pass either: the toolchain refused the question.
                println!(
                    "    {} packages record flags this toolchain rejects; they were not verified",
                    r.unresolved.len()
                );
                for pkg in r.unresolved.iter().take(3) {
                    println!("        {pkg}");
                }
            }
        }
        // Not a pass.  A rootfs debootstrap or buildroot produced records
        // nothing about how it was built, and saying "every package matches"
        // after reading none of them is the shape of bug this check exists to
        // catch.
        None => println!(
            "ISA check ({}): no package database under {root}; nothing was verified",
            Method::Flags.label()
        ),
    }
    match &report.attrs {
        Some(r) => {
            println!(
                "ISA check ({}): {} binaries under {root}, board ISA {}",
                Method::Attributes.label(),
                r.scanned,
                join(&r.expected)
            );
            if r.scanned == 0 {
                // Not a pass, for the same reason: a rootfs whose binaries
                // carry no section headers -- an OpenWrt image, say -- has no
                // `.riscv.attributes` for readelf to find, so this guard
                // provided nothing.
                println!("    no binary carried an ISA attribute; nothing was verified");
            } else if r.findings.is_empty() {
                println!("    every binary matches the board");
            } else {
                // Advisory whatever it says: an extension here can be a
                // runtime-dispatched path that the CPU never enters.
                print_findings(&r.findings, "binaries");
            }
        }
        None => println!(
            "ISA check ({}): not possible on {}, its ELF files record no ISA",
            Method::Attributes.label(),
            report.arch
        ),
    }
    fatal
}

#[cfg(test)]
mod tests {
    use super::{machine_flags, parse_arch, parse_macros, parse_records, strip_version};

    #[test]
    fn a_version_suffix_is_not_part_of_the_name() {
        assert_eq!(strip_version("zicsr2p0"), "zicsr");
        assert_eq!(strip_version("i2p1"), "i");
        // Digits that belong to the extension itself stay.
        assert_eq!(strip_version("zvl256b"), "zvl256b");
        assert_eq!(strip_version("zvl256b1p0"), "zvl256b");
        assert_eq!(strip_version("c"), "c");
    }

    #[test]
    fn an_attribute_string_reduces_to_its_extensions() {
        let set = parse_arch("rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0_zifencei2p0");
        assert!(set.contains("i") && set.contains("c") && set.contains("zifencei"));
        assert!(!set.iter().any(|e| e.starts_with("rv")));
        assert_eq!(set.len(), 8);
    }

    #[test]
    fn a_board_without_compressed_instructions_shows_c_as_extra() {
        let board = parse_arch("rv32i2p1_m2p0_a2p1");
        let binary = parse_arch("rv32i2p1_m2p0_a2p1_c2p0");
        let extra: Vec<_> = binary.difference(&board).cloned().collect();
        assert_eq!(extra, vec!["c".to_string()]);
    }

    #[test]
    fn only_machine_flags_reach_the_probe() {
        // Recorded by portage in the riscv64 target and on odroid-xu4.
        assert_eq!(
            machine_flags("-O3 -march=rva22u64_v_zvl128b -pipe -std=gnu17"),
            "-march=rva22u64_v_zvl128b"
        );
        assert_eq!(
            machine_flags("-O2 -march=armv7ve -mtune=cortex-a15.cortex-a7 -mfpu=neon-vfpv4 -mfloat-abi=hard -pipe"),
            "-march=armv7ve -mtune=cortex-a15.cortex-a7 -mfpu=neon-vfpv4 -mfloat-abi=hard"
        );
        // 290 of the 380 packages in that target look like this.
        assert_eq!(machine_flags("-O2 -pipe"), "");
    }

    #[test]
    fn a_macro_dump_reduces_to_its_feature_macros() {
        let set = parse_macros(
            "#define __SSE2__ 1\n\
             #define __ARM_FEATURE_FMA 1\n\
             #define __linux__ 1\n\
             #define __CHAR_BIT__ 8\n\
             #define __INT8_TYPE__ signed char\n\
             #define __riscv_v_intrinsic 12000\n",
        );
        assert_eq!(set.len(), 2);
        assert!(set.contains("__SSE2__") && set.contains("__ARM_FEATURE_FMA"));
    }

    #[test]
    fn a_package_built_for_a_newer_cpu_shows_up_as_extra() {
        // Verbatim from `gcc -m32 -march=pentium3 -dM -E -x c /dev/null` and
        // the same with -march=pentium4 (gcc 14.2.0), which is the Dell
        // Dimension 4100 hole: SSE2 faults on a Coppermine.
        let board = parse_macros(
            "#define __ELF__ 1\n#define __FXSR__ 1\n#define __i386 1\n\
             #define __i386__ 1\n#define __MMX__ 1\n#define __SSE__ 1\n",
        );
        let package = parse_macros(
            "#define __ELF__ 1\n#define __FXSR__ 1\n#define __i386 1\n\
             #define __i386__ 1\n#define __MMX__ 1\n#define __SSE__ 1\n\
             #define __SSE2__ 1\n",
        );
        let extra: Vec<_> = package.difference(&board).cloned().collect();
        assert_eq!(extra, vec!["__SSE2__"]);
        assert!(board.difference(&package).next().is_none());
    }

    #[test]
    fn a_record_line_is_a_key_and_a_value() {
        let records = parse_records("sys-apps/coreutils-9.7\t-O2 -pipe\nnot a record\n");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1, "-O2 -pipe");
    }
}
