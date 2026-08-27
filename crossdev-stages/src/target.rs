use camino::{Utf8Path, Utf8PathBuf};

use crate::board::BoardConfig;
use crate::container::{destroy_dir, unpack_tarball};
use crate::error::{Error, Result};
use crate::portage::{MakeConf, Portage};
use crate::sandbox::Sandbox;
use crate::stage::{chost_for_arch, default_cflags};
use crate::workspace::Workspace;

/// A cross-compiled Gentoo stage for the target arch.
pub struct Target {
    pub dir: Utf8PathBuf,
    pub arch: String,
}

/// What one target operation compiles with, and where the result is kept.
///
/// Held together on purpose: chost, CFLAGS and gcc spec are one answer, and
/// an operation that took the board's chost with the arch's flags would key
/// a store the board does not resolve to and build for hours into it.
/// (Six boards do resolve to the arch baseline -- vsrves01 states it
/// verbatim, and the five qemu-riscv64-* provider examples state no
/// BOARD_CFLAGS at all -- so for those the boardless key is correct.)
struct BuildKey {
    chost: String,
    cflags: String,
    /// Keys the crossdev prefix in the store.  CFLAGS alone.
    hash: String,
    /// Keys the binary package cache.  Also the board's per-package
    /// WORKAROUND_CFLAGS, which change what some packages are compiled with
    /// without changing the compiler.
    binpkgs_key: String,
    gcc_spec: String,
}

impl BuildKey {
    /// Everything a board decides.  The gcc spec is passed in because only
    /// the sandbox can answer it, which is also the whole of what this
    /// cannot be tested without.
    fn resolve(arch: &str, board: Option<&BoardConfig>, gcc_spec: String) -> Result<Self> {
        Ok(match board {
            Some(b) => Self {
                chost: b.chost(),
                cflags: b.effective_cflags(),
                hash: crate::cflags::toolchain_key(b),
                binpkgs_key: crate::cflags::binpkg_key(b),
                gcc_spec,
            },
            None => {
                let cflags = default_cflags(arch).to_string();
                let (_, hash) = crate::cflags::canonicalize(&cflags);
                Self {
                    chost: chost_for_arch(arch)?,
                    cflags,
                    binpkgs_key: hash.clone(),
                    hash,
                    gcc_spec,
                }
            }
        })
    }

    /// The three strings the store, the runner and the emerge command line
    /// all need, borrowed together so no caller can pair them with another
    /// key's.
    fn parts(&self) -> (&str, &str, &str) {
        (&self.chost, &self.hash, &self.gcc_spec)
    }

    /// Everything that decides what gets built, in one line, so a wrong
    /// answer is read before the hours rather than out of the binaries after.
    fn describe(&self) -> String {
        let (canonical, _) = crate::cflags::canonicalize(&self.cflags);
        format!(
            "CHOST={} CFLAGS={canonical:?} gcc-{} (store key {})",
            self.chost, self.gcc_spec, self.hash
        )
    }
}

impl Target {
    pub fn open(dir: Utf8PathBuf) -> Result<Self> {
        let arch = std::fs::read_to_string(dir.join(".arch"))
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::TargetNotFound(dir.to_string()))?;
        Ok(Self { dir, arch })
    }

    /// Create a new target stage by unpacking a stage3 source tarball (catalyst: `source_path`).
    /// Writes `.arch`, `.stage3` (file name only, for manifest) and `.provider` markers.
    pub fn create(ws: &Workspace, name: &str, arch: &str, source_stage: &Utf8Path) -> Result<Self> {
        let dir = ws.target(name);
        if dir.is_dir() {
            guard_provider(&dir, name, "gentoo")?;
            tracing::info!("Target {} already exists, skipping unpack.", name);
            return Self::open(dir);
        }
        tracing::info!("Unpacking stage3 into target {}…", dir);
        unpack_tarball(source_stage, &dir, ws.base())?;
        std::fs::write(dir.join(".arch"), arch)?;
        if let Some(fname) = source_stage.file_name() {
            std::fs::write(dir.join(".stage3"), fname)?;
        }
        std::fs::write(dir.join(".provider"), "gentoo")?;
        tracing::info!("Target {} created.", name);
        Ok(Self {
            dir,
            arch: arch.to_string(),
        })
    }

    /// Create an empty target: no stage3 seed, only the markers.  For
    /// rootfs providers that fill /target themselves; `.stage3` is set
    /// to "none" so the build lock records the absence instead of
    /// falling back to a cached tarball name, and `.provider` keeps the
    /// dir from ever being resolved for another provider's board.
    pub fn create_empty(ws: &Workspace, name: &str, arch: &str, provider: &str) -> Result<Self> {
        let dir = ws.target(name);
        if dir.is_dir() {
            guard_provider(&dir, name, provider)?;
            tracing::info!("Target {} already exists, skipping.", name);
            return Self::open(dir);
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(".arch"), arch)?;
        std::fs::write(dir.join(".stage3"), "none")?;
        std::fs::write(dir.join(".provider"), provider)?;
        tracing::info!("Target {} created (empty).", name);
        Ok(Self {
            dir,
            arch: arch.to_string(),
        })
    }

    /// The key an operation on this target builds under.
    ///
    /// With a board, exactly what the image pipeline resolves, so both write
    /// into the same store and the same binpkg cache.  Without one, the arch
    /// baseline: a fine thing to bootstrap a bare target with, and never what
    /// a board declares.
    fn build_key(&self, sandbox: &Sandbox, board: Option<&BoardConfig>) -> Result<BuildKey> {
        // Resolved the same way the image pipeline resolves it: a native
        // prefix is keyed by the compiler that actually ran, not by a slot
        // nothing in it was built with.
        let gcc_spec = match board {
            Some(b) => sandbox.gcc_spec_for(b, None)?,
            None => sandbox.default_gcc_spec()?,
        };
        BuildKey::resolve(&self.arch, board, gcc_spec)
    }

    /// Bootstrap the target: cross-emerge baselayout → packages.build → portage.
    /// Idempotent via `.stage1` marker.
    pub fn build_stage1(
        &self,
        ws: &Workspace,
        sandbox: &Sandbox,
        board: Option<&BoardConfig>,
    ) -> Result<()> {
        if self.dir.join(".stage1").exists() {
            tracing::info!("Stage1 already built, skipping.");
            return Ok(());
        }
        let key = self.build_key(sandbox, board)?;
        let (chost, hash, gcc_spec) = key.parts();
        self.announce("Bootstrapping target", board, &key);
        let binpkgs_dir = ws.binpkgs_dir().join(chost).join(&key.binpkgs_key);
        std::fs::create_dir_all(&binpkgs_dir)?;

        tracing::info!("Preparing target portage configuration…");
        self.prepare_portage_with_cflags(ws, chost, &key.cflags, gcc_spec)?;

        let runner = sandbox
            .runner_for_chost(ws, &self.arch, chost, hash, gcc_spec)?
            .with_target(&self.dir)
            .with_binpkgs(&binpkgs_dir);
        tracing::info!("Logs at: {}", runner.log_dir());
        let portage = Portage::new(&runner);

        tracing::info!("Cross-emerging baselayout…");
        portage.cross_emerge_build(chost, &["sys-apps/baselayout"])?;

        tracing::info!("Cross-emerging packages.build…");
        // rv32-musl: sys-apps/net-tools needs linux/rose.h, which musl does
        // not ship, and the rest of packages.build has never been
        // cross-emerged for that profile.  Drop net-tools (busybox provides
        // the same utilities) and report failures instead of aborting on the
        // first.  Elsewhere a packages.build failure is a real regression.
        let packages = portage.packages_build()?;
        let mut atoms: Vec<&str> = packages.split_whitespace().collect();
        if self.arch == "riscv32" {
            atoms.retain(|a| *a != "sys-apps/net-tools");
            atoms.insert(0, "--keep-going");
        }
        portage.cross_emerge(&chost, &atoms)?;

        tracing::info!("Cross-emerging portage…");
        portage.cross_emerge_build(chost, &["sys-apps/portage"])?;

        self.update_ldconfig(ws, sandbox, board)?;

        std::fs::write(self.dir.join(".stage1"), chrono::Utc::now().to_rfc3339())?;
        tracing::info!("Stage1 complete.");
        Ok(())
    }

    /// Update the target stage (`@world` rebuild).
    ///
    /// `board` decides what this compiles with.  It is optional because a
    /// target can exist without one, not because the choice is unimportant:
    /// the arch baseline is what most boards do not declare, so the key is logged
    /// either way before anything is built.
    pub fn update(
        &self,
        ws: &Workspace,
        sandbox: &Sandbox,
        board: Option<&BoardConfig>,
    ) -> Result<()> {
        let key = self.build_key(sandbox, board)?;
        let (chost, hash, gcc_spec) = key.parts();
        self.announce("Updating target", board, &key);
        let binpkgs_dir = ws.binpkgs_dir().join(chost).join(&key.binpkgs_key);
        std::fs::create_dir_all(&binpkgs_dir)?;

        // With a board, leave the target's make.conf saying what the rebuild
        // was done with -- the same file the image pipeline writes.  Without
        // one, write nothing: replacing a board's CFLAGS with the arch
        // baseline would make the target lie about its own contents.
        if board.is_some() {
            self.prepare_portage_with_cflags(ws, chost, &key.cflags, gcc_spec)?;
        }

        let runner = sandbox
            .runner_for_chost(ws, &self.arch, chost, hash, gcc_spec)?
            .with_target(&self.dir)
            .with_binpkgs(&binpkgs_dir);
        let portage = Portage::new(&runner);

        // Update the cross-toolchain in the crossdev prefix first (no ROOT=/target).
        // Pin gcc to the spec this store was built with so portage doesn't
        // pick the current default (could be a different major and would break
        // ABI of binpkgs already in the cache).  Pass --noreplace so the cross
        // prefix's package.mask/pin-gcc, which blocks upgrades past the
        // installed version, does not abort the run when gcc is already there.
        // No quoting: the atom is its own argv element (run_emerge_argv).
        let gcc_atom = format!("=sys-devel/gcc-{gcc_spec}*");
        tracing::info!(gcc_atom = %gcc_atom, "Updating crossdev prefix: gcc, binutils-libs, @system…");
        portage.cross_emerge_crossdev(chost, &["--noreplace", &gcc_atom])?;
        portage.cross_emerge_crossdev(chost, &["sys-libs/binutils-libs"])?;
        portage.cross_emerge_crossdev(chost, &["-u", "system"])?;

        tracing::info!("Rebuilding @world in target…");
        portage.cross_emerge_world(&chost)?;

        self.update_ldconfig(ws, sandbox, board)?;
        std::fs::write(self.dir.join(".updated"), chrono::Utc::now().to_rfc3339())?;
        Ok(())
    }

    /// Cross-emerge specific packages into the target.
    pub fn install(
        &self,
        ws: &Workspace,
        sandbox: &Sandbox,
        board: Option<&BoardConfig>,
        packages: &[&str],
    ) -> Result<()> {
        let key = self.build_key(sandbox, board)?;
        let (chost, hash, gcc_spec) = key.parts();
        self.announce("Installing into target", board, &key);
        let binpkgs_dir = ws.binpkgs_dir().join(chost).join(&key.binpkgs_key);
        std::fs::create_dir_all(&binpkgs_dir)?;
        let runner = sandbox
            .runner_for_chost(ws, &self.arch, chost, hash, gcc_spec)?
            .with_target(&self.dir)
            .with_binpkgs(&binpkgs_dir);
        let portage = Portage::new(&runner);
        portage.cross_emerge(chost, packages)
    }

    /// Run `ldconfig` inside the target stage.
    pub fn update_ldconfig(
        &self,
        ws: &Workspace,
        sandbox: &Sandbox,
        board: Option<&BoardConfig>,
    ) -> Result<()> {
        tracing::info!("Updating ldconfig in target…");
        let key = self.build_key(sandbox, board)?;
        let runner = sandbox
            .runner_for_chost(ws, &self.arch, &key.chost, &key.hash, &key.gcc_spec)?
            .with_target(&self.dir);
        runner.run("ldconfig -v -r /target")
    }

    /// Say which board's answer an operation took, and which flags follow
    /// from it, before the operation starts.
    fn announce(&self, what: &str, board: Option<&BoardConfig>, key: &BuildKey) {
        match board {
            Some(b) => tracing::info!("{what} for board {}: {}", b.name, key.describe()),
            None => tracing::info!(
                "{what} with the {} defaults, which most boards do not declare: {}. \
                 Pass --board to build what a board asks for.",
                self.arch,
                key.describe()
            ),
        }
    }

    /// Write target portage make.conf with explicit CFLAGS and copy the
    /// profile link from the crossdev prefix in the workspace store —
    /// mirrors `prepare_target_portage` in the bash script.  Idempotent:
    /// re-writes make.conf each call so callers can refresh CFLAGS when
    /// a board's values change.
    pub fn prepare_portage_with_cflags(
        &self,
        ws: &Workspace,
        chost: &str,
        cflags: &str,
        gcc_spec: &str,
    ) -> Result<()> {
        let portage_dir = self.dir.join("etc/portage");
        std::fs::create_dir_all(&portage_dir)?;

        // pkgdir is deliberately None: this make.conf ships into the image
        // via `cp -a /target/. /build/gen/root/`.  FEATURES=buildpkg +
        // PKGDIR=/binpkgs belong in the crossdev prefix config, which is
        // what {chost}-emerge (PORTAGE_CONFIGROOT=/usr/<chost>) reads.
        MakeConf {
            arch: &self.arch,
            chost: Some(chost),
            cflags: Some(cflags),
            mirror: None,
            binhost: None,
            pkgdir: None,
            for_build_host: false,
        }
        .write(&portage_dir)?;

        // The store key already resolved which gcc this target is built by.
        crate::portage::write_version_pins(&portage_dir, Some(gcc_spec))?;

        // Copy the profile directory and make.profile symlink from the
        // store-resident crossdev prefix so the target stage uses the
        // correct Gentoo profile.  The (chost, cflags-hash, gcc-spec)
        // keyed store dir is the source of truth post-Phase 3.
        let (_, hash) = crate::cflags::canonicalize(cflags);
        let src_portage = ws
            .store_dir()
            .join(crate::workspace::store_key(chost, &hash, gcc_spec))
            .join("etc/portage");

        let src_profile_dir = src_portage.join("profile");
        if src_profile_dir.is_dir() {
            let dst = portage_dir.join("profile");
            // Replace, do not merge: this directory is a copy of the prefix's,
            // so anything still in it is left over from an older build.  `cp -a
            // src dst` puts the source *inside* dst once dst exists, which is
            // how targets on this machine grew an etc/portage/profile/profile.
            // Copying the contents keeps that from coming back even when the
            // removal fails.
            let _ = std::fs::remove_dir_all(&dst);
            std::fs::create_dir_all(&dst)?;
            let status = std::process::Command::new("cp")
                .args(["-a", &format!("{src_profile_dir}/."), dst.as_str()])
                .status()?;
            if !status.success() {
                return Err(Error::CommandFailed {
                    code: status.code().unwrap_or(-1),
                    reason: format!("cp -a {src_profile_dir} failed"),
                });
            }
        }

        let src_link = src_portage.join("make.profile");
        let dst_link = portage_dir.join("make.profile");
        if src_link.is_symlink() {
            // A real (non-symlink) /bin marks a deliberately split-usr
            // target: keep whatever profile it has (copy verbatim only if
            // none is set).  Merged-usr and fresh (pre-baselayout) targets
            // get the translated link, overwritten every time — a stale
            // symlink in a re-bootstrapped target would silently use the
            // wrong profile.
            let split_usr_target = std::fs::symlink_metadata(self.dir.join("bin"))
                .map(|m| !m.file_type().is_symlink())
                .unwrap_or(false);
            let link_target = std::fs::read_link(&src_link)?;
            let translated = translate_profile_link(&link_target, split_usr_target);
            if split_usr_target {
                if !dst_link.is_symlink() && !dst_link.exists() {
                    std::os::unix::fs::symlink(&translated, &dst_link)?;
                }
            } else {
                let _ = std::fs::remove_file(&dst_link);
                std::os::unix::fs::symlink(&translated, &dst_link)?;
            }
        }

        Ok(())
    }
}

/// Refuse to adopt an existing target dir that another rootfs provider
/// created: silently reusing it hands e.g. an empty provider-owned tree
/// to the Gentoo pipeline (garbage image) or a stage3 tree to a
/// provider that would provision over it.
fn guard_provider(dir: &camino::Utf8Path, name: &str, provider: &str) -> Result<()> {
    let have = crate::workspace::read_provider(dir);
    if have != provider {
        return Err(Error::CommandFailed {
            code: 1,
            reason: format!(
                "target '{name}' belongs to rootfs provider '{have}', not '{provider}' \
                 -- pass --target or `target destroy {name}` first"
            ),
        });
    }
    Ok(())
}

/// Profile symlink to plant in the target, derived from the crossdev
/// prefix's link.  Crossdev prefixes use split-usr profiles (the prefix
/// layout mandates it — /usr/{chost}/lib is a sibling of /usr/{chost}/
/// usr/lib); a merged-usr target must drop the `split-usr` segment or
/// baselayout creates /bin as a real dir and every subsequent emerge
/// collides.  A split-usr target keeps the link verbatim.
fn translate_profile_link(link: &std::path::Path, split_usr_target: bool) -> std::path::PathBuf {
    if split_usr_target {
        link.to_path_buf()
    } else {
        link.to_string_lossy().replace("/split-usr/", "/").into()
    }
}

/// Remove a target directory (via hakoniwa to handle root-owned files).
pub fn destroy(ws: &Workspace, name: &str) -> Result<()> {
    let dir = ws.target(name);
    if !dir.is_dir() {
        return Err(Error::TargetNotFound(name.into()));
    }
    println!("Removing target: {name}");
    destroy_dir(&dir, ws.base())?;
    println!("Target '{name}' removed.");
    Ok(())
}

/// List all target directories with their state.
pub fn list(ws: &Workspace) -> Result<Vec<TargetInfo>> {
    let dirs = ws.list_targets()?;
    Ok(dirs
        .into_iter()
        .map(|dir| {
            let arch = crate::workspace::read_arch(&dir).unwrap_or_else(|| "unknown".into());
            let stage1 = dir.join(".stage1").exists();
            let updated = std::fs::read_to_string(dir.join(".updated"))
                .ok()
                .map(|s| s.trim().to_string());
            let name = dir.file_name().unwrap_or("").to_string();
            TargetInfo {
                name,
                arch,
                stage1,
                updated,
            }
        })
        .collect())
}

pub struct TargetInfo {
    pub name: String,
    pub arch: String,
    pub stage1: bool,
    pub updated: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{translate_profile_link, BuildKey};
    use crate::board::BoardConfig;
    use std::path::Path;

    /// dell-dimension-4100: an i686 board whose CFLAGS are nothing like the
    /// i686 default (`-O3 -pipe`, the catch-all arm of `default_cflags`).
    fn pentium3() -> BoardConfig {
        let mut b = crate::cli::util::default_board_config("i686");
        b.name = "dell-dimension-4100".into();
        b.cflags = Some("-O2 -march=pentium3 -pipe".into());
        b
    }

    /// The bug this exists to stop: keying an hours-long rebuild on flags no
    /// board asked for.  The two must not be the same key.
    #[test]
    fn a_board_does_not_key_the_arch_default() {
        let board = pentium3();
        let with = BuildKey::resolve("i686", Some(&board), "16".into()).unwrap();
        let without = BuildKey::resolve("i686", None, "16".into()).unwrap();
        assert_eq!(with.cflags, "-O2 -march=pentium3 -pipe");
        assert_eq!(without.cflags, "-O3 -pipe");
        assert_ne!(with.hash, without.hash);
    }

    /// The same store and the same binpkg cache an image build of that board
    /// uses, or the rebuild lands where the board never reads.
    #[test]
    fn the_key_is_the_one_the_image_pipeline_resolves() {
        let board = pentium3();
        let key = BuildKey::resolve("i686", Some(&board), "16.2.9999".into()).unwrap();
        assert_eq!(key.hash, crate::cflags::toolchain_key(&board));
        assert_eq!(key.binpkgs_key, crate::cflags::binpkg_key(&board));
        assert_eq!(key.chost, board.chost());
    }

    /// A per-package workaround changes what some packages are compiled
    /// with, not what the compiler is: binpkgs move, the toolchain does not.
    #[test]
    fn a_workaround_moves_only_the_binpkg_cache() {
        let mut board = pentium3();
        board.workaround_pkgs = vec!["sys-libs/glibc".into()];
        board.workaround_cflags = vec!["-O2 -march=i686 -pipe".into()];
        let key = BuildKey::resolve("i686", Some(&board), "16".into()).unwrap();
        let plain = BuildKey::resolve("i686", Some(&pentium3()), "16".into()).unwrap();
        assert_eq!(key.hash, plain.hash);
        assert_ne!(key.binpkgs_key, plain.binpkgs_key);
    }

    /// CHOST is the board's when it overrides one; deriving it from the arch
    /// again would key a store the board never resolves to.
    #[test]
    fn a_chost_override_is_the_chost() {
        let mut board = pentium3();
        board.chost_override = Some("i686-pc-linux-musl".into());
        let key = BuildKey::resolve("i686", Some(&board), "16".into()).unwrap();
        assert_eq!(key.chost, "i686-pc-linux-musl");
    }

    /// Without a board there is no board's answer to take, and the line has
    /// to name the flags before the hours are spent.
    #[test]
    fn the_boardless_key_describes_itself() {
        let key = BuildKey::resolve("riscv64", None, "16".into()).unwrap();
        let line = key.describe();
        // Canonical form, not the source order: that is what keys the store.
        assert!(line.contains("-march=rv64gc"), "{line}");
        assert!(line.contains("gcc-16"), "{line}");
        assert!(line.contains(&key.hash), "{line}");
    }

    #[test]
    fn profile_link_drops_split_usr_for_merged_target() {
        let link = Path::new(
            "../../var/db/repos/gentoo/profiles/default/linux/riscv/23.0/split-usr/rv32/ilp32/musl",
        );
        assert_eq!(
            translate_profile_link(link, false),
            Path::new("../../var/db/repos/gentoo/profiles/default/linux/riscv/23.0/rv32/ilp32/musl")
        );
    }

    #[test]
    fn profile_link_kept_verbatim_for_split_usr_target() {
        let link = Path::new(
            "../../var/db/repos/gentoo/profiles/default/linux/riscv/23.0/split-usr/rv32/ilp32/musl",
        );
        assert_eq!(translate_profile_link(link, true), link);
    }

    #[test]
    fn profile_link_without_split_usr_segment_is_unchanged() {
        let link =
            Path::new("../../var/db/repos/gentoo/profiles/default/linux/riscv/23.0/rv64/lp64d");
        assert_eq!(translate_profile_link(link, false), link);
    }
}
