//! The bundled default zone (`zone/` in the repository), compiled into the
//! binary so `oss-server` runs with no external files. See `zone/README.md`.

use std::path::Path;

use crate::config::MemSource;

/// Every file of the default zone: `(zone-relative path, contents)`.
pub const FILES: &[(&str, &[u8])] = &[
    (
        "arenas/(default)/arena.conf",
        include_bytes!("../../../zone/arenas/(default)/arena.conf"),
    ),
    (
        "arenas/(default)/balls.conf",
        include_bytes!("../../../zone/arenas/(default)/balls.conf"),
    ),
    (
        "arenas/(default)/flags.conf",
        include_bytes!("../../../zone/arenas/(default)/flags.conf"),
    ),
    (
        "arenas/(public)/README.public",
        include_bytes!("../../../zone/arenas/(public)/README.public"),
    ),
    // the LVZ's source and licence travel with it, so a zone written out
    // with --init-zone carries the attribution too
    (
        "arenas/scf/README.lvz",
        include_bytes!("../../../zone/arenas/scf/README.lvz"),
    ),
    (
        "arenas/scf/arena.conf",
        include_bytes!("../../../zone/arenas/scf/arena.conf"),
    ),
    (
        "arenas/scf/scf-demo.lvz",
        include_bytes!("../../../zone/arenas/scf/scf-demo.lvz"),
    ),
    (
        "arenas/scf/scf.lvl",
        include_bytes!("../../../zone/arenas/scf/scf.lvl"),
    ),
    (
        "conf/global.conf",
        include_bytes!("../../../zone/conf/global.conf"),
    ),
    (
        "conf/groupdef.conf",
        include_bytes!("../../../zone/conf/groupdef.conf"),
    ),
    (
        "conf/groupdef.dir/alias",
        include_bytes!("../../../zone/conf/groupdef.dir/alias"),
    ),
    (
        "conf/groupdef.dir/default",
        include_bytes!("../../../zone/conf/groupdef.dir/default"),
    ),
    (
        "conf/groupdef.dir/mod",
        include_bytes!("../../../zone/conf/groupdef.dir/mod"),
    ),
    (
        "conf/groupdef.dir/objects",
        include_bytes!("../../../zone/conf/groupdef.dir/objects"),
    ),
    (
        "conf/groupdef.dir/smod",
        include_bytes!("../../../zone/conf/groupdef.dir/smod"),
    ),
    (
        "conf/groupdef.dir/sysop",
        include_bytes!("../../../zone/conf/groupdef.dir/sysop"),
    ),
    (
        "conf/modules.conf",
        include_bytes!("../../../zone/conf/modules.conf"),
    ),
    (
        "conf/passwd.conf",
        include_bytes!("../../../zone/conf/passwd.conf"),
    ),
    (
        "conf/staff.conf",
        include_bytes!("../../../zone/conf/staff.conf"),
    ),
    (
        "conf/svs/cost",
        include_bytes!("../../../zone/conf/svs/cost"),
    ),
    (
        "conf/svs/misc",
        include_bytes!("../../../zone/conf/svs/misc"),
    ),
    (
        "conf/svs/prizeweights",
        include_bytes!("../../../zone/conf/svs/prizeweights"),
    ),
    (
        "conf/svs/ship-javelin",
        include_bytes!("../../../zone/conf/svs/ship-javelin"),
    ),
    (
        "conf/svs/ship-lancaster",
        include_bytes!("../../../zone/conf/svs/ship-lancaster"),
    ),
    (
        "conf/svs/ship-leviathan",
        include_bytes!("../../../zone/conf/svs/ship-leviathan"),
    ),
    (
        "conf/svs/ship-shark",
        include_bytes!("../../../zone/conf/svs/ship-shark"),
    ),
    (
        "conf/svs/ship-spider",
        include_bytes!("../../../zone/conf/svs/ship-spider"),
    ),
    (
        "conf/svs/ship-terrier",
        include_bytes!("../../../zone/conf/svs/ship-terrier"),
    ),
    (
        "conf/svs/ship-warbird",
        include_bytes!("../../../zone/conf/svs/ship-warbird"),
    ),
    (
        "conf/svs/ship-weasel",
        include_bytes!("../../../zone/conf/svs/ship-weasel"),
    ),
    (
        "conf/svs/svs.conf",
        include_bytes!("../../../zone/conf/svs/svs.conf"),
    ),
    (
        "maps/smallmap.lvl",
        include_bytes!("../../../zone/maps/smallmap.lvl"),
    ),
    ("news.txt", include_bytes!("../../../zone/news.txt")),
];

/// The default zone as an in-memory source (changes made while running,
/// e.g. `?passwd`, are not saved).
pub fn source() -> MemSource {
    MemSource::new(FILES.iter().map(|(p, d)| (*p, *d)))
}

/// Writes the default zone into `dir` (creating it). Refuses to touch a
/// directory that already has a `conf/global.conf`.
pub fn write_to(dir: &Path) -> std::io::Result<()> {
    if dir.join("conf/global.conf").exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already contains a zone", dir.display()),
        ));
    }
    for (rel, data) in FILES {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded list is exactly the `zone/` directory (minus its README).
    #[test]
    fn embedded_files_match_the_zone_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../zone");
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).expect("read_dir") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p
                        .strip_prefix(&root)
                        .expect("prefix")
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel != "README.md" {
                        on_disk.push(rel);
                    }
                }
            }
        }
        on_disk.sort();
        let embedded: Vec<String> = FILES.iter().map(|(p, _)| (*p).to_owned()).collect();
        assert_eq!(embedded, on_disk, "regenerate src/default_zone.rs");
    }
}
