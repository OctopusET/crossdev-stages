use crate::cli::util::{default_board_config, ensure_crossdev, ensure_target};
use crate::cli::TargetCmd;
use crate::error::Result;
use crate::{board, container, stage, target, workspace::Workspace};
use camino::{Utf8Path, Utf8PathBuf};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    ws: &Workspace,
    arch: Option<String>,
    sandbox: Option<String>,
    target_name: Option<String>,
    board_name: Option<String>,
    cmd: TargetCmd,
    boards_root: &Utf8Path,
    defaults_root: &Utf8Path,
    mirror: Option<&str>,
) -> Result<()> {
    // Loaded once, up front: a board that does not parse must not be found
    // out after a stage3 has been fetched and unpacked.
    let board_cfg = match &board_name {
        Some(name) => Some(board::load(boards_root, name)?),
        None => None,
    };
    let board = board_cfg.as_ref();
    // A board names its own arch; --arch stays for the boardless case and
    // still wins when both are given, so nothing that worked stops working.
    let arch = arch.or_else(|| board.map(|b| b.arch.clone()));

    match cmd {
        TargetCmd::List => {
            for t in target::list(ws)? {
                let s1 = if t.stage1 { "stage1" } else { "unpacked" };
                let upd = t.updated.as_deref().unwrap_or("-");
                println!(
                    "{:<20} arch={} state={} updated={}",
                    t.name, t.arch, s1, upd
                );
            }
        }
        TargetCmd::Setup { name, from } => {
            let (resolved_arch, source_stage) = if let Some(local) = from {
                let a = arch.ok_or_else(|| crate::error::Error::CommandFailed {
                    code: 1,
                    reason: "--arch is required when using --from".into(),
                })?;
                (a, local)
            } else {
                let a = arch.ok_or_else(|| crate::error::Error::CommandFailed {
                    code: 1,
                    reason: "--arch is required for target setup".into(),
                })?;
                let f = stage::fetch(&ws.stages_dir(), &a, mirror).await?;
                (a, f)
            };
            let name = name.unwrap_or_else(|| {
                format!(
                    "{resolved_arch}-{}",
                    chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
                )
            });
            target::Target::create(ws, &name, &resolved_arch, &source_stage)?;
            println!("Target '{name}' created.");
            let fallback = default_board_config(&resolved_arch);
            ensure_crossdev(
                ws,
                sandbox.as_deref(),
                &resolved_arch,
                board.unwrap_or(&fallback),
                defaults_root,
                mirror,
                None,
            )
            .await?;
        }
        TargetCmd::Stage1 => {
            let (tgt, sb) = ensure_target(
                ws,
                target_name.as_deref(),
                arch.as_deref(),
                sandbox.as_deref(),
                board,
                defaults_root,
                mirror,
            )
            .await?;
            tgt.build_stage1(ws, &sb, board)?;
        }
        TargetCmd::Update => {
            let (tgt, sb) = ensure_target(
                ws,
                target_name.as_deref(),
                arch.as_deref(),
                sandbox.as_deref(),
                board,
                defaults_root,
                mirror,
            )
            .await?;
            tgt.update(ws, &sb, board)?;
        }
        TargetCmd::Install { packages } => {
            let (tgt, sb) = ensure_target(
                ws,
                target_name.as_deref(),
                arch.as_deref(),
                sandbox.as_deref(),
                board,
                defaults_root,
                mirror,
            )
            .await?;
            let pkgs: Vec<&str> = packages.iter().map(String::as_str).collect();
            tgt.install(ws, &sb, board, &pkgs)?;
        }
        TargetCmd::Ldconfig => {
            let (tgt, sb) = ensure_target(
                ws,
                target_name.as_deref(),
                arch.as_deref(),
                sandbox.as_deref(),
                board,
                defaults_root,
                mirror,
            )
            .await?;
            tgt.update_ldconfig(ws, &sb, board)?;
        }
        TargetCmd::Destroy { name } => {
            target::destroy(ws, &name)?;
        }
        TargetCmd::Export {
            output,
            compression,
        } => {
            let tgt_dir = ws.resolve_target(target_name.as_deref())?;
            let tgt = target::Target::open(tgt_dir)?;
            let tgt_name = tgt.dir.file_name().unwrap_or("target");
            let ext = match compression.as_str() {
                "gz" | "gzip" => "tar.gz",
                "none" => "tar",
                _ => "tar.xz",
            };
            let out_path = output.unwrap_or_else(|| {
                Utf8PathBuf::from(format!("stage3-{}-{}.{ext}", tgt.arch, tgt_name))
            });
            println!("Packing target '{}' -> {out_path} ...", tgt_name);
            container::pack_tarball(&tgt.dir, &out_path, ws.base(), &compression)?;
            let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
            println!("Done: {out_path} ({:.1}M)", size as f64 / 1_048_576.0);
        }
    }
    Ok(())
}
