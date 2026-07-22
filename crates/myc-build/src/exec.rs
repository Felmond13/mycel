//! `[[build.run]]` execution: materialize the current build state into a
//! throwaway rootfs, run the step in a rootless container, then scan the
//! rootfs back into the entry tree.
//!
//! CRITICAL: the rootfs is materialized with COPIES, never hardlinks.
//! `myc_run::materialize` hardlinks blobs straight out of the store — a
//! build step that modifies a file in place would then be writing to the
//! store's own inode, corrupting every environment that shares it. Copies
//! cost a few MB of I/O per step and are always correct; builds are not the
//! hot path.

use crate::scan::{scan_rootfs, ScanOutcome};
use crate::spec::RunStep;
use crate::tree::FileTree;
use crate::{BuildError, Result};
use myc_manifest::{EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct StepSummary {
    pub command: Vec<String>,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub bytes_added: u64,
    pub blobs_added: u64,
    pub duration: Duration,
}

fn io_err(path: &Path, source: io::Error) -> BuildError {
    BuildError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Materialize `tree` into `rootfs` with full copies (see module docs).
/// Directories are created writable first and receive their exact mode at
/// the end, so restrictive dir modes cannot block their own population.
pub fn materialize_copy(store: &Store, tree: &FileTree, rootfs: &Path) -> Result<()> {
    fs::create_dir_all(rootfs).map_err(|e| io_err(rootfs, e))?;
    let mut dir_modes: Vec<(std::path::PathBuf, u32)> = Vec::new();

    for entry in tree.values() {
        let dest = rootfs.join(entry.path.trim_start_matches('/'));
        if let Some(parent) = dest.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
            }
        }
        match entry.kind {
            EntryKind::Dir => {
                if !dest.exists() {
                    fs::create_dir_all(&dest).map_err(|e| io_err(&dest, e))?;
                }
                set_mode(&dest, entry.mode | 0o700)?;
                dir_modes.push((dest, entry.mode));
            }
            EntryKind::File => {
                let blob = store.blob(entry.blake3.as_deref().unwrap())?;
                fs::copy(&blob, &dest).map_err(|e| io_err(&dest, e))?;
                set_mode(&dest, entry.mode)?;
            }
            EntryKind::Symlink => {
                let target = entry.target.as_deref().unwrap();
                let _ = fs::remove_file(&dest);
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &dest).map_err(|e| io_err(&dest, e))?;
                #[cfg(not(unix))]
                return Err(BuildError::Spec(format!(
                    "cannot create symlink {target} on this platform"
                )));
            }
            // Devices come from the runtime's /dev; FIFOs are recreated by
            // the apps that need them (same policy as myc-run).
            EntryKind::Fifo | EntryKind::CharDevice | EntryKind::BlockDevice => {}
        }
    }
    // Deepest-first so a restrictive parent mode lands after its children's.
    for (dir, mode) in dir_modes.into_iter().rev() {
        set_mode(&dir, mode)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| io_err(path, e))?;
    }
    let _ = (path, mode);
    Ok(())
}

/// Run one build step against `tree`, updating it in place.
///
/// `config` supplies the environment and working directory the step sees
/// (the build's final merged config, so `[config] workdir` behaves like
/// Docker's WORKDIR for RUN).
pub fn run_step(
    store: &Store,
    tree: &mut FileTree,
    config: &RuntimeConfig,
    step: &RunStep,
    index: usize,
    work_dir: &Path,
) -> Result<StepSummary> {
    let start = Instant::now();
    let step_dir = work_dir.join(format!("step-{index}"));
    let rootfs = step_dir.join("rootfs");
    materialize_copy(store, tree, &rootfs)?;

    // A minimal manifest for the runner: empty entrypoint/cmd so the step's
    // command is exec'd verbatim (a base entrypoint must not wrap RUN steps).
    let step_manifest = Manifest {
        schema: SCHEMA_VERSION,
        name: format!("build-step-{index}"),
        origin: None,
        os: "linux".to_string(),
        arch: std::env::consts::ARCH.to_string(),
        created: "1970-01-01T00:00:00Z".to_string(),
        config: RuntimeConfig {
            env: config.env.clone(),
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            workdir: config.workdir.clone(),
            user: String::new(),
            exposed_ports: Vec::new(),
            volumes: Vec::new(),
        },
        entries: Vec::new(), // unused: the rootfs is already built
    };
    let options = myc_run::RunOptions {
        command: step.command.clone(),
        env: step.env.clone(),
        hostname: "mycel-build".to_string(),
        binds: Vec::new(),
        workdir: None,
        keep_rootfs: true, // we own the rootfs lifecycle
        network: myc_run::Network::Host,
        ..Default::default()
    };

    let code = myc_run::exec_prepared(&step_manifest, &rootfs, &options)?;
    if code != 0 {
        remove_tree(&step_dir);
        return Err(BuildError::StepFailed { index, code });
    }

    let outcome: ScanOutcome = scan_rootfs(store, &rootfs, tree)?;
    remove_tree(&step_dir);

    Ok(StepSummary {
        command: step.command.clone(),
        added: outcome.added,
        modified: outcome.modified,
        removed: outcome.removed,
        bytes_added: outcome.bytes_added,
        blobs_added: outcome.blobs_added,
        duration: start.elapsed(),
    })
}

/// Best-effort recursive removal; image dir modes may lack write permission.
fn remove_tree(dir: &Path) {
    make_removable(dir);
    let _ = fs::remove_dir_all(dir);
}

fn make_removable(dir: &Path) {
    let _ = set_mode(dir, 0o755);
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && !p.is_symlink() {
                make_removable(&p);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tree::insert;
    use myc_manifest::Entry;

    #[test]
    fn copy_materialization_never_links_store_inodes() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let (hash, size) = store.put_blob(&mut &b"mutable content"[..]).unwrap();

        let mut tree = FileTree::new();
        insert(
            &mut tree,
            Entry {
                path: "/data".into(),
                kind: EntryKind::File,
                mode: 0o644,
                uid: 0,
                gid: 0,
                size,
                blake3: Some(hash.clone()),
                target: None,
                device: None,
            },
        );
        let rootfs = dir.path().join("rootfs");
        materialize_copy(&store, &tree, &rootfs).unwrap();

        let store_ino = fs::metadata(store.blob(&hash).unwrap()).unwrap().ino();
        let copy_meta = fs::metadata(rootfs.join("data")).unwrap();
        assert_ne!(copy_meta.ino(), store_ino, "must be a copy, not a link");
        assert_eq!(copy_meta.nlink(), 1);

        // Mutating the copy must not touch the store blob.
        fs::write(rootfs.join("data"), b"scribbled").unwrap();
        store.verify_blob(&hash).unwrap();
    }

    #[test]
    fn restrictive_dir_modes_do_not_block_population() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let (hash, size) = store.put_blob(&mut &b"x"[..]).unwrap();
        let mut tree = FileTree::new();
        insert(
            &mut tree,
            Entry {
                path: "/locked".into(),
                kind: EntryKind::Dir,
                mode: 0o555,
                uid: 0,
                gid: 0,
                size: 0,
                blake3: None,
                target: None,
                device: None,
            },
        );
        insert(
            &mut tree,
            Entry {
                path: "/locked/f".into(),
                kind: EntryKind::File,
                mode: 0o644,
                uid: 0,
                gid: 0,
                size,
                blake3: Some(hash),
                target: None,
                device: None,
            },
        );
        let rootfs = dir.path().join("rootfs");
        materialize_copy(&store, &tree, &rootfs).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(rootfs.join("locked"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o555);
        assert!(rootfs.join("locked/f").exists());
        remove_tree(&rootfs);
        assert!(!rootfs.exists());
    }
}
