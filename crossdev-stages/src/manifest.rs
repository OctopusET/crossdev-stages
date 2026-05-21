//! Image manifest sidecar — emits `<image>.manifest.json` listing
//! board, image hash, and partition table (offset/size/source/sha256)
//! after `pack` step succeeds.  Useful for verifying image integrity
//! and for partial-write to eMMC/SPI flash at known offsets.

use camino::Utf8Path;
use serde::Serialize;
use std::process::Command;

use crate::error::Result;

#[derive(Serialize)]
pub struct Manifest {
    pub board: String,
    pub image: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub built_at: String,
    pub partitions: Vec<Partition>,
}

#[derive(Serialize)]
pub struct Partition {
    pub name: String,
    pub offset: Option<String>,
    pub size: Option<String>,
    pub image: Option<String>,
    pub sha256: Option<String>,
}

pub fn write(
    build_dir: &Utf8Path,
    board: &str,
    image_name: &str,
    genimage_cfg: Option<&Utf8Path>,
) -> Result<()> {
    let img_path = build_dir.join(image_name);
    let size_bytes = std::fs::metadata(&img_path)?.len();
    let sha256 = sha256_file(&img_path)?;

    let partitions = match genimage_cfg {
        Some(p) if p.exists() => parse_partitions(&std::fs::read_to_string(p)?)
            .into_iter()
            .map(|mut part| {
                if let Some(src) = &part.image {
                    let abs = build_dir.join(src);
                    if abs.exists() {
                        part.sha256 = sha256_file(&abs).ok();
                    }
                }
                part
            })
            .collect(),
        _ => Vec::new(),
    };

    let manifest = Manifest {
        board: board.to_string(),
        image: image_name.to_string(),
        size_bytes,
        sha256,
        built_at: chrono::Utc::now().to_rfc3339(),
        partitions,
    };

    let out = build_dir.join(format!("{image_name}.manifest.json"));
    std::fs::write(&out, serde_json::to_string_pretty(&manifest)?)?;
    Ok(())
}

fn sha256_file(path: &Utf8Path) -> Result<String> {
    let output = Command::new("sha256sum").arg(path.as_str()).output()?;
    Ok(output
        .stdout
        .split(|&b| b == b' ')
        .next()
        .map(|s| String::from_utf8_lossy(s).to_string())
        .unwrap_or_default())
}

/// Parse `partition NAME { key = "value" ... }` blocks from every
/// `image NAME { ... }` block of a genimage config.  (genimage allows
/// multiple top-level images — rootfs.ext4, bootfs, then the final
/// hdimage with partitions.)
fn parse_partitions(cfg: &str) -> Vec<Partition> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_image = false;
    let mut current: Option<Partition> = None;

    for raw in cfg.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("/*") {
            continue;
        }

        if !in_image && line.starts_with("image ") && line.ends_with('{') {
            in_image = true;
            depth = 1;
            continue;
        }
        if !in_image {
            continue;
        }

        if let Some(name) = line
            .strip_prefix("partition ")
            .and_then(|s| s.strip_suffix(" {"))
        {
            current = Some(Partition {
                name: name.trim().to_string(),
                offset: None,
                size: None,
                image: None,
                sha256: None,
            });
            depth += 1;
            continue;
        }

        if line == "}" {
            depth -= 1;
            if let Some(p) = current.take() {
                out.push(p);
            }
            if depth == 0 {
                in_image = false;
            }
            continue;
        }

        if line.ends_with('{') {
            depth += 1;
            continue;
        }

        if let Some(p) = current.as_mut() {
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim().trim_matches('"').trim_end_matches(';').trim_matches('"');
                match k.trim() {
                    "offset" => p.offset = Some(v.to_string()),
                    "size" if !v.is_empty() => p.size = Some(v.to_string()),
                    "image" => p.image = Some(v.to_string()),
                    _ => {}
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_preceding_filesystem_images() {
        let cfg = r#"
image rootfs.ext4 {
    ext4 { label = "rootfs" }
    size = 5G
}

image bootfs.fat32 {
    vfat { label = "boot" }
    size = 128M
}

image sdcard.img {
    hdimage { partition-table-type = gpt }
    partition rootfs {
        image = "rootfs.ext4"
        offset = "131M"
    }
}
"#;
        let parts = parse_partitions(cfg);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "rootfs");
    }

    #[test]
    fn parses_k230_layout() {
        let cfg = r#"
image foo.img {
    hdimage { partition-table-type = gpt }
    partition uboot_spl_1 {
        image = "u-boot/fn_u-boot-spl.bin"
        offset = "1024K"
        size = "512K"
    }
    partition rootfs {
        image = "rootfs.ext4"
        offset = "131M"
        size = ""
        partition-type-uuid = "root-riscv64"
    }
}
"#;
        let parts = parse_partitions(cfg);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "uboot_spl_1");
        assert_eq!(parts[0].offset.as_deref(), Some("1024K"));
        assert_eq!(parts[0].image.as_deref(), Some("u-boot/fn_u-boot-spl.bin"));
        assert_eq!(parts[1].name, "rootfs");
        assert_eq!(parts[1].size, None);
    }
}
