use camino::Utf8Path;

use crate::container::SandboxRunner;
use crate::error::Result;
use crate::stage::{all_llvm_targets, default_cflags, gentoo_arch, llvm_target};

/// Write package.mask/pin-gcc + package.unmask/pin-gcc + same-for-llvm into
/// a portage root (host sandbox, cross-prefix, or target sysroot).
///
/// `gcc_version` Some(v) → pin gcc to `=sys-devel/gcc-${v}*` (mask all others).
/// `gcc_version` None    → leave gcc alone (host sandbox: stage3 default).
///
/// `llvm_slot` always pins llvm-core/* to the given slot (`=...-${slot}*`).
/// This is a single string like `"22"` — keeping a fixed slot prevents
/// multi-slot llvm installs that would bloat the rootfs and confuse
/// llvm-config / clang-driver discovery.
/// Single supported llvm slot.  Bumping requires manually verifying the
/// host stage3 + crossdev binhost have matching slot binpkgs available.
pub const LLVM_SLOT: &str = "22";

pub fn write_version_pins(
    portage_root: &Utf8Path,
    gcc_version: Option<&str>,
) -> Result<()> {
    let llvm_slot = LLVM_SLOT;
    let mask_dir = portage_root.join("package.mask");
    let unmask_dir = portage_root.join("package.unmask");
    std::fs::create_dir_all(&mask_dir)?;
    std::fs::create_dir_all(&unmask_dir)?;

    // Remove legacy single-file format we used before this helper existed.
    let _ = std::fs::remove_file(mask_dir.join("llvm-unused-slot"));

    if let Some(v) = gcc_version {
        std::fs::write(mask_dir.join("pin-gcc"), "sys-devel/gcc\n")?;
        std::fs::write(
            unmask_dir.join("pin-gcc"),
            format!("=sys-devel/gcc-{v}*\n"),
        )?;
    } else {
        let _ = std::fs::remove_file(mask_dir.join("pin-gcc"));
        let _ = std::fs::remove_file(unmask_dir.join("pin-gcc"));
    }

    const LLVM_PKGS: &[&str] = &[
        "clang", "clang-common", "clang-toolchain-symlinks",
        "clang-linker-config", "lld", "lld-toolchain-symlinks",
        "llvm", "llvm-common", "llvmgold", "llvm-toolchain-symlinks",
    ];
    let mask: String = LLVM_PKGS.iter()
        .map(|p| format!("llvm-core/{p}\n"))
        .collect();
    let unmask: String = LLVM_PKGS.iter()
        .map(|p| format!("=llvm-core/{p}-{llvm_slot}*\n"))
        .collect();
    std::fs::write(mask_dir.join("pin-llvm"), mask)?;
    std::fs::write(unmask_dir.join("pin-llvm"), unmask)?;

    Ok(())
}

/// Parameters for a Portage `make.conf` file.
pub struct MakeConf<'a> {
    pub arch: &'a str,
    pub chost: Option<&'a str>,
    pub cflags: Option<&'a str>,
    pub mirror: Option<&'a str>,
    pub binhost: Option<&'a str>,
    /// True for sandbox-internal portage configs (host + cross-prefix).
    /// Enables `PORTAGE_TMPDIR=/dev/shm` for faster builds inside the
    /// hakoniwa container — never set on target sysroots, where the
    /// booted system would inherit `/dev/shm` as a noexec tmpfs and
    /// emerge would fail with "Can not execute files in /dev/shm".
    pub sandbox_internal: bool,
}

impl<'a> MakeConf<'a> {
    /// Write `make.conf` into `portage_dir` (i.e. `/etc/portage` of a sandbox or target stage).
    /// Updates variables in-place; preserves any existing content not managed here.
    pub fn write(&self, portage_dir: &Utf8Path) -> Result<()> {
        std::fs::create_dir_all(portage_dir)?;
        std::fs::create_dir_all(portage_dir.join("package.accept_keywords"))?;
        std::fs::create_dir_all(portage_dir.join("package.mask"))?;

        let make_conf = portage_dir.join("make.conf");
        if !make_conf.exists() {
            std::fs::write(&make_conf, "")?;
        }

        // Rust pins itself to llvm_slot_21 via REQUIRED_USE, so llvm:22 is
        // unreachable here. Without this mask, llvm-21's `>=llvmgold-21` dep
        // resolves to llvmgold-22 (newest), which drags in the full llvm:22
        // chain for nothing.
        std::fs::write(
            portage_dir.join("package.mask/llvm-unused-slot"),
            ">=llvm-core/llvmgold-22\n\
             >=llvm-core/llvm-common-22\n",
        )?;

        let (jobs, load) = parallelism();
        let garch = gentoo_arch(self.arch)?;
        let cflags = self.cflags.unwrap_or_else(|| default_cflags(self.arch));

        set_make_conf_var(&make_conf, "MAKEOPTS", &format!("-j{jobs} --load-average {load}"))?;
        set_make_conf_var(
            &make_conf,
            "EMERGE_DEFAULT_OPTS",
            &format!("--jobs={jobs} --load-average {load}"),
        )?;
        set_make_conf_var(
            &make_conf,
            "FEATURES",
            "parallel-install parallel-fetch -merge-wait pkgdir-index-trusted",
        )?;
        if self.sandbox_internal {
            // Container already tmpfs-mounts /dev/shm; using it for portage's
            // build dir avoids disk IO on big builds (gcc, llvm, rust).
            // Skip on target sysroots — the booted system's /dev/shm is a
            // noexec tmpfs and emerge would refuse to run ebuild.sh there.
            set_make_conf_var(&make_conf, "PORTAGE_TMPDIR", "/dev/shm")?;
        }
        set_make_conf_var(&make_conf, "ACCEPT_KEYWORDS", &format!("~{garch}"))?;
        set_make_conf_var(&make_conf, "PORT_LOGDIR", &format!("/var/log/portage/{garch}"))?;

        // LLVM_TARGETS: host gets the union of every supported arch (so the
        // bundled LLVM inside dev-lang/rust can bootstrap any cross-std);
        // cross-sysroots get only their own arch target.
        let llvm_targets = match self.chost {
            Some(_) => llvm_target(self.arch).map(str::to_string),
            None => Some(all_llvm_targets()),
        };
        if let Some(targets) = llvm_targets.filter(|s| !s.is_empty()) {
            set_make_conf_var(&make_conf, "LLVM_TARGETS", &targets)?;
        }

        if let Some(chost) = self.chost {
            set_make_conf_var(&make_conf, "CHOST", chost)?;
            set_make_conf_var(&make_conf, "CFLAGS", cflags)?;
            set_make_conf_var(&make_conf, "CXXFLAGS", cflags)?;
        }

        if let Some(mirror) = self.mirror {
            set_make_conf_var(&make_conf, "GENTOO_MIRRORS", mirror)?;
        }

        if let Some(binhost) = self.binhost {
            set_make_conf_var(&make_conf, "PORTAGE_BINHOST", binhost)?;
            let features = "parallel-install -merge-wait getbinpkg";
            set_make_conf_var(&make_conf, "FEATURES", features)?;
        }

        Ok(())
    }
}

pub fn parallelism() -> (usize, usize) {
    let n = num_cpus::get();
    (n, n * 2)
}

/// Set or replace a variable in a make.conf file.
/// If the variable exists, replace its value; otherwise append.
pub fn set_make_conf_var(file: &Utf8Path, name: &str, value: &str) -> Result<()> {
    let content = std::fs::read_to_string(file).unwrap_or_default();
    let prefix = format!("{name}=");
    let new_line = format!("{name}=\"{value}\"");

    let mut found = false;
    let mut lines: Vec<String> = content
        .lines()
        .map(|line| {
            if line.starts_with(&prefix) {
                found = true;
                new_line.clone()
            } else {
                line.to_string()
            }
        })
        .collect();

    if !found {
        lines.push(new_line);
    }

    std::fs::write(file, lines.join("\n") + "\n")?;
    Ok(())
}

/// Portage operations that run *inside* a sandbox container.
pub struct Portage<'a> {
    runner: &'a SandboxRunner,
}

impl<'a> Portage<'a> {
    pub fn new(runner: &'a SandboxRunner) -> Self {
        Self { runner }
    }

    /// Initial sync of the portage tree.
    pub fn webrsync(&self) -> Result<()> {
        self.runner.run("emerge-webrsync")
    }

    /// `getuto` — fetch binary package signing keys (best-effort).
    pub fn getuto(&self) -> Result<()> {
        // Ignore failures: getuto may not be available or may fail on first run.
        let _ = self.runner.run("getuto");
        Ok(())
    }

    /// Emerge packages from binary packages only (`-G`).
    pub fn emerge_binary(&self, packages: &[&str]) -> Result<()> {
        let pkgs = packages.join(" ");
        self.runner.run(&format!("emerge -G {pkgs}"))
    }

    /// Emerge packages, using binary if available (`-b -k`).
    pub fn emerge(&self, packages: &[&str]) -> Result<()> {
        let pkgs = packages.join(" ");
        self.runner.run(&format!("emerge -b -k {pkgs}"))
    }

    /// Rebuild the world set.
    #[allow(dead_code)]
    pub fn emerge_world(&self) -> Result<()> {
        self.runner.run("emerge -b -k -e @world")
    }

    /// Cross-emerge packages into the target stage (mounted at `/target`).
    /// Uses `{chost}-emerge` which crossdev installs.
    pub fn cross_emerge(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let pkgs = packages.join(" ");
        self.runner
            .run(&format!("ROOT=/target {chost}-emerge -b -k {pkgs}"))
    }

    /// Cross-emerge with `USE=build` for bootstrapping (baselayout, portage).
    pub fn cross_emerge_build(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let pkgs = packages.join(" ");
        self.runner
            .run(&format!("USE=build ROOT=/target {chost}-emerge -b -k {pkgs}"))
    }

    /// Run `{chost}-emerge` without overriding ROOT, so packages install into
    /// the crossdev prefix (`/usr/{chost}`) rather than `/target`.
    /// Used for updating the cross-toolchain itself (gcc, binutils-libs, @system).
    pub fn cross_emerge_crossdev(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let pkgs = packages.join(" ");
        self.runner.run(&format!("{chost}-emerge -b -k {pkgs}"))
    }
}

/// Install all host-side dependencies required for cross-compilation.
pub fn install_host_deps(runner: &SandboxRunner) -> Result<()> {
    let portage = Portage::new(runner);

    tracing::info!("Syncing portage tree…");
    portage.webrsync()?;
    let _ = portage.getuto();

    runner.run("chown -R portage:portage /etc/portage/gnupg")?;

    let bin_packages = ["app-arch/zstd", "app-arch/bzip2", "app-arch/xz-utils"];
    tracing::info!("Installing binary packages…");
    portage.emerge_binary(&bin_packages)?;

    let packages = [
        "sys-devel/crossdev",
        "sys-devel/bc",
        "sys-apps/merge-usr",
        "dev-vcs/git",
        "dev-embedded/u-boot-tools",
        "sys-apps/dtc",
        "sys-kernel/dracut",
        "sys-apps/busybox",
        "sys-fs/genimage",
        "sys-fs/dosfstools",
        "sys-fs/mtools",
        "app-eselect/eselect-repository",
        "dev-lang/rust",
        "dev-python/pyelftools",
    ];
    tracing::info!("Installing build dependencies…");
    portage.emerge(&packages)?;

    tracing::info!("Installing Rust ldconfig…");
    runner.run("cargo install --root /usr/local ldconfig")?;

    Ok(())
}

