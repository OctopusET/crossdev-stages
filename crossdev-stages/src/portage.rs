use camino::Utf8Path;

use crate::container::SandboxRunner;
use crate::error::Result;
use crate::stage::{all_llvm_targets, default_cflags, gentoo_arch, llvm_target};

/// Single supported llvm slot; dev-lang/rust-1.95.0 has `LLVM_COMPAT=( 22 )`.
pub const LLVM_SLOT: &str = "22";

/// Write package.mask/pin-gcc + package.unmask/pin-gcc + same-for-llvm into
/// a portage root (host sandbox, cross-prefix, or target sysroot).
///
/// `gcc_version` Some(v) → pin gcc to `=sys-devel/gcc-${v}*` (mask all others).
/// `gcc_version` None    → leave gcc alone (host sandbox: stage3 default).
///
/// llvm-core/* is always pinned to [`LLVM_SLOT`] (`=...-${slot}*`) —
/// keeping a fixed slot prevents multi-slot llvm installs that would
/// bloat the rootfs and confuse llvm-config / clang-driver discovery.
pub fn write_version_pins(portage_root: &Utf8Path, gcc_version: Option<&str>) -> Result<()> {
    let llvm_slot = LLVM_SLOT;
    let mask_dir = portage_root.join("package.mask");
    let unmask_dir = portage_root.join("package.unmask");
    std::fs::create_dir_all(&mask_dir)?;
    std::fs::create_dir_all(&unmask_dir)?;

    // Remove legacy single-file format we used before this helper existed.
    let _ = std::fs::remove_file(mask_dir.join("llvm-unused-slot"));

    if let Some(v) = gcc_version {
        std::fs::write(mask_dir.join("pin-gcc"), "sys-devel/gcc\n")?;
        std::fs::write(unmask_dir.join("pin-gcc"), format!("=sys-devel/gcc-{v}*\n"))?;
    } else {
        let _ = std::fs::remove_file(mask_dir.join("pin-gcc"));
        let _ = std::fs::remove_file(unmask_dir.join("pin-gcc"));
    }

    const LLVM_PKGS: &[&str] = &[
        "clang",
        "clang-common",
        "clang-toolchain-symlinks",
        "clang-linker-config",
        "lld",
        "lld-toolchain-symlinks",
        "llvm",
        "llvm-common",
        "llvmgold",
        "llvm-toolchain-symlinks",
    ];
    let mask: String = LLVM_PKGS
        .iter()
        .map(|p| format!("llvm-core/{p}\n"))
        .collect();
    let unmask: String = LLVM_PKGS
        .iter()
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
    /// In-container path of a writable bind-mount where binpkgs should
    /// land.  When set, `FEATURES` gains `buildpkg` and `PKGDIR` is
    /// pointed at it.  `None` keeps portage defaults.
    pub pkgdir: Option<&'a str>,
    /// Whether emerges driven by this config run on the machine doing the
    /// build.  The sandbox and the crossdev prefix do; the target stage does
    /// not -- it is carried onto the board, where this machine's core count
    /// and log paths are not just useless but harmful.
    pub for_build_host: bool,
}

impl<'a> MakeConf<'a> {
    /// Write `make.conf` into `portage_dir` (i.e. `/etc/portage` of a sandbox or target stage).
    /// Updates variables in-place; preserves any existing content not managed here.
    pub fn write(&self, portage_dir: &Utf8Path) -> Result<()> {
        std::fs::create_dir_all(portage_dir)?;
        std::fs::create_dir_all(portage_dir.join("package.accept_keywords"))?;

        let make_conf = portage_dir.join("make.conf");
        if !make_conf.exists() {
            std::fs::write(&make_conf, "")?;
        }

        let garch = gentoo_arch(self.arch)?;
        let cflags = self.cflags.unwrap_or_else(|| default_cflags(self.arch));

        set_make_conf_var(&make_conf, "ACCEPT_KEYWORDS", &format!("~{garch}"))?;

        if self.for_build_host {
            let (jobs, load) = parallelism();
            set_make_conf_var(
                &make_conf,
                "MAKEOPTS",
                &format!("-j{jobs} --load-average {load}"),
            )?;
            set_make_conf_var(
                &make_conf,
                "EMERGE_DEFAULT_OPTS",
                &format!("--jobs={jobs} --load-average {load}"),
            )?;
            // buildpkg only where there is a keyed directory to build into:
            // a binary package is only safe to reuse under the flags that
            // made it, and that is what PKGDIR names.
            let features = if self.pkgdir.is_some() {
                "parallel-install parallel-fetch -merge-wait pkgdir-index-trusted buildpkg"
            } else {
                "parallel-install parallel-fetch -merge-wait pkgdir-index-trusted"
            };
            set_make_conf_var(&make_conf, "FEATURES", features)?;
            // The container already tmpfs-mounts /dev/shm, and portage's build
            // dir is the one thing in a cross build that is pure write-then-
            // discard: gcc, llvm and rust each move gigabytes through it.
            set_make_conf_var(&make_conf, "PORTAGE_TMPDIR", "/dev/shm")?;
            // PORTAGE_LOGDIR, not PORT_LOGDIR.  Portage renamed it and the
            // old name is inert: it named a directory in every failure
            // message while writing nothing into it, so the build log an
            // emerge failure points at never existed and
            // SHOW_BUILD_FAILURES had nothing to find.  /var/log is a
            // read-write bind of the workspace's logs/<sandbox>/, so what
            // lands here outlives the container.
            set_make_conf_var(
                &make_conf,
                "PORTAGE_LOGDIR",
                &format!("/var/log/portage/{garch}"),
            )?;
            unset_make_conf_var(&make_conf, "PORT_LOGDIR")?;
        } else {
            // Older target stages were written with this machine's tuning in
            // them.  Take it back out rather than leave a board emerging with
            // a build host's core count.
            for stale in [
                "MAKEOPTS",
                "EMERGE_DEFAULT_OPTS",
                "FEATURES",
                "PORT_LOGDIR",
                "PORTAGE_LOGDIR",
            ] {
                unset_make_conf_var(&make_conf, stale)?;
            }
        }

        // PKGDIR names a bind-mount that exists only inside the sandbox, and
        // the target's make.conf is copied into the image.  Drop a stale line
        // when unset so a target that once carried one is healed.
        match self.pkgdir {
            Some(pkgdir) => set_make_conf_var(&make_conf, "PKGDIR", pkgdir)?,
            None => remove_make_conf_var(&make_conf, "PKGDIR")?,
        }

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
            let features = if self.pkgdir.is_some() {
                "parallel-install -merge-wait buildpkg getbinpkg"
            } else {
                "parallel-install -merge-wait getbinpkg"
            };
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

/// Remove a variable from a make.conf file if present.
fn remove_make_conf_var(file: &Utf8Path, name: &str) -> Result<()> {
    let content = std::fs::read_to_string(file).unwrap_or_default();
    let prefix = format!("{name}=");
    let lines: Vec<&str> = content
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .collect();
    std::fs::write(file, lines.join("\n") + "\n")?;
    Ok(())
}

/// Print the tail of every recent portage log that recorded a failure.
/// Falls back to naming the newest logs when nothing matched, so the failure
/// is never reported with no way to look further.
const SHOW_BUILD_FAILURES: &str = r#"
logs=$(ls -t /var/log/portage/*/*.log 2>/dev/null | head -n 40)
[ -n "$logs" ] || exit 0
hit=$(grep -lE " \\* ERROR: |failed \\(.* phase\\)" $logs 2>/dev/null | head -n 3)
if [ -n "$hit" ]; then
    for f in $hit; do
        printf "\n--- %s ---\n" "$f"
        tail -n 80 "$f"
    done
else
    printf "\nNo build log recorded a failure.  Most recent logs:\n"
    printf "%s\n" "$logs" | head -n 5
fi
"#;

/// Remove a variable from a make.conf, leaving everything else alone.
pub fn unset_make_conf_var(file: &Utf8Path, name: &str) -> Result<()> {
    let content = std::fs::read_to_string(file).unwrap_or_default();
    let prefix = format!("{name}=");
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .collect();
    std::fs::write(file, kept.join("\n") + "\n")?;
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

    /// Run an emerge and, if it fails, print the build log that recorded why.
    ///
    /// Portage switches to background mode whenever EMERGE_DEFAULT_OPTS carries
    /// `--jobs` (Scheduler._background_mode), so a failing build writes nothing
    /// to stdout; the compiler output only ever reaches PORT_LOGDIR.  That
    /// directory is owned by the sandbox uid and mode 0770, so it cannot be
    /// read from the host either -- the tail has to be taken from inside.
    fn run_emerge(&self, cmd: &str) -> Result<()> {
        let result = self.runner.run(cmd);
        if result.is_err() {
            let _ = self.runner.run(SHOW_BUILD_FAILURES);
        }
        result
    }

    /// [`Self::run_emerge`] for an argv: each atom is its own argument, so
    /// `>=`, `:` and a trailing `*` reach portage untouched, with no shell
    /// to quote them against.  Still a login shell: hakoniwa execs without a
    /// PATH search, and /etc/profile is where gcc-config and crossdev put
    /// theirs.
    fn run_emerge_argv(&self, head: &[&str], packages: &[&str]) -> Result<()> {
        let argv: Vec<&str> = ["/bin/bash", "--login", "-c", "exec \"$@\"", "bash"]
            .iter()
            .chain(head)
            .chain(packages)
            .copied()
            .collect();
        let result = self.runner.run_argv(&argv);
        if result.is_err() {
            let _ = self.runner.run(SHOW_BUILD_FAILURES);
        }
        result
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

    /// Emerge packages, using binary if available (`-b -k`).
    /// `--changed-use` so a board's `sandbox-packages.use` also applies to
    /// packages that are already installed with different flags.
    pub fn emerge(&self, packages: &[&str]) -> Result<()> {
        self.run_emerge_argv(&["emerge", "-b", "-k", "--changed-use"], packages)
    }

    /// Rebuild the world set.
    #[allow(dead_code)]
    pub fn emerge_world(&self) -> Result<()> {
        self.run_emerge("emerge -b -k -e @world")
    }

    /// Bring the world set up to date: newer versions, changed USE, and the
    /// dependencies that follow from either.  `--keep-going` because one
    /// package failing to build is not a reason to leave the other ninety
    /// un-updated, and the log of what failed is printed either way.
    pub fn update_world(&self) -> Result<()> {
        self.run_emerge("emerge -b -k -uDN --keep-going --with-bdeps=y @world")
    }

    /// Bring a set of packages up to date without touching the rest.
    pub fn update(&self, packages: &[&str]) -> Result<()> {
        self.run_emerge_argv(&["emerge", "-b", "-k", "-uDN", "--keep-going"], packages)
    }

    /// Same, cross-emerged into the target stage.
    pub fn cross_update(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let emerge = format!("{chost}-emerge");
        self.run_emerge_argv(
            &["env", "ROOT=/target", &emerge, "-b", "-k", "-uDN", "--keep-going"],
            packages,
        )
    }

    /// Cross-emerge packages into the target stage (mounted at `/target`).
    /// Uses `{chost}-emerge` which crossdev installs.
    pub fn cross_emerge(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let emerge = format!("{chost}-emerge");
        self.run_emerge_argv(&["env", "ROOT=/target", &emerge, "-b", "-k"], packages)
    }

    /// Cross-emerge with `USE=build` for bootstrapping (baselayout, portage).
    pub fn cross_emerge_build(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let emerge = format!("{chost}-emerge");
        self.run_emerge_argv(&["env", "USE=build", "ROOT=/target", &emerge, "-b", "-k"], packages)
    }

    /// Rebuild the target stage's `@world` with `KERNEL_DIR` pointing at the
    /// in-prefix kernel headers, which some `@world` packages read.
    /// Explicit --jobs / --load-average so EMERGE_DEFAULT_OPTS from make.conf
    /// can't be silently lost (e.g. when a wrapper drops PORTAGE_CONFIGROOT).
    pub fn cross_emerge_world(&self, chost: &str) -> Result<()> {
        let (jobs, load) = parallelism();
        self.runner.run(&format!(
            "KERNEL_DIR=/usr/src/linux ROOT=/target {chost}-emerge \
             -b -k --jobs={jobs} --load-average {load} -e @world"
        ))
    }

    /// `packages.build` lines from the target profile's gentoo repo, joined
    /// for a single cross-emerge. Fails if the file is missing or empty.
    pub fn packages_build(&self) -> Result<String> {
        let packages = self.runner.run_output(
            "grep -v '^#' /var/db/repos/gentoo/profiles/default/linux/packages.build \
             | grep -v '^[[:space:]]*$' | tr '\\n' ' '",
        )?;
        if packages.is_empty() {
            return Err(crate::error::Error::CommandFailed {
                code: 1,
                reason: "packages.build is empty or missing".into(),
            });
        }
        Ok(packages)
    }

    /// Rebuild exactly `atoms` into /target from source.
    ///
    /// Deliberately not `-e <set>`.  `-e` also re-merges what the cross
    /// prefix already holds, and the prefix is an overlayfs mount: portage
    /// ends a merge by renaming `<pkg>-MERGING` into place, which returns EIO
    /// on a rootless overlay when the target exists in the lower layer
    /// (userxattr mode cannot write the redirect xattr a cross-layer
    /// directory move needs).  Without `-e`, satisfied prefix packages are
    /// left alone.  Explicit --jobs / --load-average for the reason
    /// `cross_emerge_world` gives.
    pub fn cross_emerge_rebuild(&self, chost: &str, atoms: &[String]) -> Result<()> {
        let (jobs, load) = parallelism();
        let emerge = format!("{chost}-emerge");
        let jobs = format!("--jobs={jobs}");
        let load = load.to_string();
        let atoms: Vec<&str> = atoms.iter().map(String::as_str).collect();
        self.run_emerge_argv(
            &[
                "env", "ROOT=/target", &emerge, "-b", "--usepkg=n", "--oneshot",
                "--keep-going", &jobs, "--load-average", &load,
            ],
            &atoms,
        )
    }

    /// Run `{chost}-emerge` without overriding ROOT, so packages install into
    /// the crossdev prefix (`/usr/{chost}`) rather than `/target`.
    /// Used for updating the cross-toolchain itself (gcc, binutils-libs, @system).
    pub fn cross_emerge_crossdev(&self, chost: &str, packages: &[&str]) -> Result<()> {
        let emerge = format!("{chost}-emerge");
        self.run_emerge_argv(&[&emerge, "-b", "-k"], packages)
    }

    /// Emerge the host `sys-devel/gcc` cross-compiler seed into the sandbox.
    ///
    /// `ver_prefix` selects a version glob (`=sys-devel/gcc-15.2*`, quoted so the
    /// shell cannot expand it); `None` takes the best available in the slot.
    pub fn emerge_host_gcc(&self, slot: &str, ver_prefix: Option<&str>) -> Result<()> {
        match ver_prefix {
            Some(prefix) => {
                tracing::info!("Emerging =sys-devel/gcc-{prefix}* (host)…");
                self.runner
                    .run(&format!("emerge -b -k '=sys-devel/gcc-{prefix}*'"))
            }
            None => {
                tracing::info!("Emerging sys-devel/gcc:{slot} (host)…");
                self.runner
                    .run(&format!("emerge -b -k sys-devel/gcc:{slot}"))
            }
        }
    }

    // ── Crossdev prefix setup ────────────────────────────────────────────────

    /// Create the `crossdev` overlay repository if it isn't registered yet.
    pub fn ensure_crossdev_repo(&self) -> Result<()> {
        self.runner.run(
            "eselect repository list -i | grep -q crossdev \
             || eselect repository create crossdev",
        )
    }

    /// `crossdev <chost> --init-target`: overlay alias and sysroot skeleton,
    /// no toolchain built yet.
    pub fn crossdev_init(&self, chost: &str) -> Result<()> {
        self.runner.run(&format!("crossdev {chost} --init-target"))
    }

    /// `crossdev <chost> --gcc <ver> --ex-pkg …`: the toolchain build itself.
    pub fn crossdev_setup(&self, chost: &str, gcc_ver: &str, ex_pkgs: &[&str]) -> Result<()> {
        let mut cmd = format!("crossdev {chost} --gcc {gcc_ver}");
        for pkg in ex_pkgs {
            cmd.push_str(" --ex-pkg ");
            cmd.push_str(pkg);
        }
        self.runner.run(&cmd)
    }

    /// Point `PORTAGE_CONFIGROOT` at the crossdev prefix and pick its profile.
    pub fn crossdev_set_profile(&self, chost: &str, profile: &str) -> Result<()> {
        self.runner.run(&format!(
            "export PORTAGE_CONFIGROOT=/usr/{chost}; eselect profile set {profile}"
        ))
    }

    /// Repair the split-usr layout crossdev leaves behind in the prefix.
    pub fn crossdev_merge_usr(&self, chost: &str) -> Result<()> {
        self.runner.run(&format!("mkdir -p /usr/{chost}/bin"))?;
        self.runner.run(&format!("merge-usr --root /usr/{chost}"))
    }

    /// Write one `package.accept_keywords` line into the sandbox's `/etc/portage`.
    pub fn accept_keywords(&self, name: &str, line: &str) -> Result<()> {
        self.runner.run(&format!(
            "echo '{line}' > /etc/portage/package.accept_keywords/{name}"
        ))
    }

    // ── Toolchain activation ────────────────────────────────────────────────

    /// Activate the `{chost}-{slot}` gcc profile.
    pub fn activate_gcc(&self, chost: &str, slot: &str) -> Result<()> {
        self.runner.run(&format!("gcc-config {chost}-{slot}"))
    }

    /// Re-read `/etc/profile` so an activated compiler reaches `PATH`.
    pub fn activate_gcc_and_profile(&self, chost: &str, slot: &str) -> Result<()> {
        self.runner
            .run(&format!("gcc-config {chost}-{slot} && source /etc/profile"))
    }

    /// Regenerate `/etc/profile.env` from `/etc/env.d` and re-read it.
    pub fn refresh_profile_env(&self) -> Result<()> {
        self.runner.run("env-update && source /etc/profile")
    }

    // ── Queries ─────────────────────────────────────────────────────────────

    /// The sandbox host's own `CHOST`, as `portageq` reports it.
    pub fn host_chost(&self) -> Result<String> {
        Ok(self
            .runner
            .run_output("portageq envvar CHOST")?
            .trim()
            .to_string())
    }

    /// Installed versions of `cpn`, one per line, as `qlist -ICev` reports them.
    pub fn installed_versions(&self, cpn: &str) -> Result<String> {
        self.runner.run_output(&format!("qlist -ICev {cpn}"))
    }
}

/// Sync the portage tree inside a sandbox (emerge-webrsync, signing keys).
pub fn sync_portage_tree(runner: &SandboxRunner) -> Result<()> {
    let portage = Portage::new(runner);

    tracing::info!("Syncing portage tree…");
    portage.webrsync()?;
    let _ = portage.getuto();

    runner.run("chown -R portage:portage /etc/portage/gnupg")?;
    Ok(())
}

/// Install all host-side dependencies required for cross-compilation.
///
/// Reads the package list from `<defaults_root>/sandbox-packages.txt`. Per-board
/// extras (boards/<board>/sandbox-packages.txt) are emerged separately during
/// image build via `image::default_deps`. Keyword overrides (`atom [keywords]`
/// lines) are written to `<portage_dir>/package.accept_keywords/`.
pub fn install_host_deps(
    runner: &SandboxRunner,
    defaults_root: &Utf8Path,
    portage_dir: &Utf8Path,
) -> Result<()> {
    sync_portage_tree(runner)?;
    let portage = Portage::new(runner);

    let path = defaults_root.join("sandbox-packages.txt");
    let packages = crate::package_list::read_required(&path)?;
    crate::package_list::write_accept_keywords(&packages, portage_dir)?;

    tracing::info!(
        "Installing host build dependencies ({} packages)…",
        packages.len()
    );
    portage.emerge(&crate::package_list::atoms(&packages))?;

    tracing::info!("Installing Rust ldconfig…");
    runner.run("cargo install --root /usr/local ldconfig")?;

    Ok(())
}
