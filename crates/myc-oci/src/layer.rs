//! OCI layer application: streams a (compressed) layer tar and folds it into
//! the in-memory file tree, storing file contents in the content-addressed
//! store as they are read. Handles OCI whiteouts (`.wh.` files and
//! `.wh..wh..opq` opaque directory markers) and hardlinks.

use crate::{OciError, Result};
use myc_manifest::{normalize_tar_path, Entry, EntryKind};
use myc_store::Store;
use std::collections::BTreeMap;
use std::io::Read;

pub struct LayerStats {
    pub files_added: usize,
    pub blobs_added: u64,
    pub bytes_added: u64,
}

/// Decompress according to media type. Docker/OCI layers are gzip, zstd or raw tar.
fn decompress<'a>(
    reader: Box<dyn Read + Send + 'a>,
    media_type: &str,
) -> Result<Box<dyn Read + 'a>> {
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        Ok(Box::new(flate2::read::GzDecoder::new(reader)))
    } else if media_type.ends_with("+zstd") || media_type.ends_with(".zstd") {
        Ok(Box::new(
            zstd::stream::read::Decoder::new(reader).map_err(OciError::Io)?,
        ))
    } else if media_type.ends_with(".tar") || media_type.ends_with("diff.tar") {
        Ok(Box::new(reader))
    } else {
        // Default for Docker Hub: layers are gzip even when the media type
        // string is unusual. Sniffing would be nicer; gzip is the safe bet.
        Ok(Box::new(flate2::read::GzDecoder::new(reader)))
    }
}

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_MARKER: &str = ".wh..wh..opq";

pub fn apply_layer(
    store: &Store,
    tree: &mut BTreeMap<String, Entry>,
    reader: Box<dyn Read + Send>,
    media_type: &str,
    _digest: &str,
) -> Result<LayerStats> {
    let decompressed = decompress(reader, media_type)?;
    let mut archive = tar::Archive::new(decompressed);
    archive.set_ignore_zeros(true);

    let mut stats = LayerStats {
        files_added: 0,
        blobs_added: 0,
        bytes_added: 0,
    };

    for entry in archive.entries().map_err(OciError::Io)? {
        let mut entry = entry.map_err(OciError::Io)?;
        let raw_path = entry
            .path()
            .map_err(OciError::Io)?
            .to_string_lossy()
            .to_string();
        let Some(path) = normalize_tar_path(&raw_path) else {
            continue;
        };

        let file_name = path.rsplit('/').next().unwrap_or_default().to_string();
        let parent = match path.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => path[..i].to_string(),
            None => "/".to_string(),
        };

        // Opaque directory: remove everything under the parent that came
        // from previous layers.
        if file_name == OPAQUE_MARKER {
            let prefix = if parent == "/" {
                "/".to_string()
            } else {
                format!("{parent}/")
            };
            tree.retain(|p, _| !(p.starts_with(&prefix) && p.as_str() != parent));
            continue;
        }

        // Whiteout: delete the shadowed path (and its subtree).
        if let Some(hidden) = file_name.strip_prefix(WHITEOUT_PREFIX) {
            let victim = if parent == "/" {
                format!("/{hidden}")
            } else {
                format!("{parent}/{hidden}")
            };
            let sub_prefix = format!("{victim}/");
            tree.retain(|p, _| p != &victim && !p.starts_with(&sub_prefix));
            continue;
        }

        let header = entry.header();
        let mode = header.mode().map_err(OciError::Io)? & 0o7777;
        let uid = header.uid().map_err(OciError::Io)? as u32;
        let gid = header.gid().map_err(OciError::Io)? as u32;

        use tar::EntryType;
        let new_entry = match header.entry_type() {
            EntryType::Directory => Entry {
                path: path.clone(),
                kind: EntryKind::Dir,
                mode,
                uid,
                gid,
                size: 0,
                blake3: None,
                target: None,
                device: None,
            },
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                let (hash, size) = store.put_blob_with_mode(&mut entry, mode)?;
                stats.files_added += 1;
                stats.blobs_added += 1;
                stats.bytes_added += size;
                Entry {
                    path: path.clone(),
                    kind: EntryKind::File,
                    mode,
                    uid,
                    gid,
                    size,
                    blake3: Some(hash),
                    target: None,
                    device: None,
                }
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(OciError::Io)?
                    .map(|p| p.to_string_lossy().to_string())
                    .ok_or_else(|| OciError::Layer(format!("symlink without target: {path}")))?;
                Entry {
                    path: path.clone(),
                    kind: EntryKind::Symlink,
                    mode: 0o777,
                    uid,
                    gid,
                    size: 0,
                    blake3: None,
                    target: Some(target),
                    device: None,
                }
            }
            EntryType::Link => {
                // Hardlink: resolve to the already-ingested target entry and
                // duplicate its identity (same blob hash).
                let target_raw = entry
                    .link_name()
                    .map_err(OciError::Io)?
                    .map(|p| p.to_string_lossy().to_string())
                    .ok_or_else(|| OciError::Layer(format!("hardlink without target: {path}")))?;
                let target_path = normalize_tar_path(&target_raw)
                    .ok_or_else(|| OciError::Layer(format!("bad hardlink target: {target_raw}")))?;
                let Some(target_entry) = tree.get(&target_path).cloned() else {
                    return Err(OciError::Layer(format!(
                        "hardlink to unknown target: {path} -> {target_path}"
                    )));
                };
                Entry {
                    path: path.clone(),
                    ..target_entry
                }
            }
            EntryType::Fifo => Entry {
                path: path.clone(),
                kind: EntryKind::Fifo,
                mode,
                uid,
                gid,
                size: 0,
                blake3: None,
                target: None,
                device: None,
            },
            EntryType::Char | EntryType::Block => {
                let major = header.device_major().map_err(OciError::Io)?.unwrap_or(0) as u64;
                let minor = header.device_minor().map_err(OciError::Io)?.unwrap_or(0) as u64;
                Entry {
                    path: path.clone(),
                    kind: if header.entry_type() == EntryType::Char {
                        EntryKind::CharDevice
                    } else {
                        EntryKind::BlockDevice
                    },
                    mode,
                    uid,
                    gid,
                    size: 0,
                    blake3: None,
                    target: None,
                    device: Some((major, minor)),
                }
            }
            // PAX headers, GNU long names etc. are consumed by the tar crate.
            _ => continue,
        };

        // A file replacing a directory shadows the whole subtree.
        if let Some(prev) = tree.get(&path) {
            if prev.kind == EntryKind::Dir && new_entry.kind != EntryKind::Dir {
                let sub_prefix = format!("{path}/");
                tree.retain(|p, _| !p.starts_with(&sub_prefix));
            }
        }
        tree.insert(path, new_entry);
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn build_tar(entries: Vec<(&str, tar::EntryType, &[u8], Option<&str>)>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind, content, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(0o755);
            header.set_uid(0);
            header.set_gid(0);
            header.set_size(content.len() as u64);
            match kind {
                tar::EntryType::Symlink | tar::EntryType::Link => {
                    header.set_size(0);
                    builder
                        .append_link(&mut header, path, link.unwrap())
                        .unwrap();
                }
                _ => {
                    builder.append_data(&mut header, path, content).unwrap();
                }
            }
        }
        builder.into_inner().unwrap()
    }

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        (dir, store)
    }

    fn apply(store: &Store, tree: &mut BTreeMap<String, Entry>, tar_bytes: Vec<u8>) {
        apply_layer(
            store,
            tree,
            Box::new(Cursor::new(tar_bytes)),
            "application/vnd.oci.image.layer.v1.tar",
            "sha256:test",
        )
        .unwrap();
    }

    #[test]
    fn basic_files_and_dirs() {
        let (_d, store) = temp_store();
        let mut tree = BTreeMap::new();
        let tar = build_tar(vec![
            ("bin", tar::EntryType::Directory, b"", None),
            ("bin/sh", tar::EntryType::Regular, b"#!binary", None),
            ("bin/link", tar::EntryType::Symlink, b"", Some("sh")),
        ]);
        apply(&store, &mut tree, tar);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree["/bin"].kind, EntryKind::Dir);
        assert_eq!(tree["/bin/sh"].kind, EntryKind::File);
        assert!(store.has_blob(tree["/bin/sh"].blake3.as_ref().unwrap()));
        assert_eq!(tree["/bin/link"].target.as_deref(), Some("sh"));
    }

    #[test]
    fn whiteout_removes_file() {
        let (_d, store) = temp_store();
        let mut tree = BTreeMap::new();
        apply(
            &store,
            &mut tree,
            build_tar(vec![
                ("etc", tar::EntryType::Directory, b"", None),
                ("etc/old.conf", tar::EntryType::Regular, b"old", None),
            ]),
        );
        apply(
            &store,
            &mut tree,
            build_tar(vec![(
                "etc/.wh.old.conf",
                tar::EntryType::Regular,
                b"",
                None,
            )]),
        );
        assert!(tree.contains_key("/etc"));
        assert!(!tree.contains_key("/etc/old.conf"));
    }

    #[test]
    fn opaque_dir_clears_subtree() {
        let (_d, store) = temp_store();
        let mut tree = BTreeMap::new();
        apply(
            &store,
            &mut tree,
            build_tar(vec![
                ("data", tar::EntryType::Directory, b"", None),
                ("data/a", tar::EntryType::Regular, b"a", None),
                ("data/b", tar::EntryType::Regular, b"b", None),
            ]),
        );
        apply(
            &store,
            &mut tree,
            build_tar(vec![
                ("data/.wh..wh..opq", tar::EntryType::Regular, b"", None),
                ("data/c", tar::EntryType::Regular, b"c", None),
            ]),
        );
        assert!(tree.contains_key("/data"));
        assert!(!tree.contains_key("/data/a"));
        assert!(!tree.contains_key("/data/b"));
        assert!(tree.contains_key("/data/c"));
    }

    #[test]
    fn hardlink_shares_blob() {
        let (_d, store) = temp_store();
        let mut tree = BTreeMap::new();
        apply(
            &store,
            &mut tree,
            build_tar(vec![
                ("bin/busybox", tar::EntryType::Regular, b"busybox!", None),
                ("bin/ls", tar::EntryType::Link, b"", Some("bin/busybox")),
            ]),
        );
        assert_eq!(
            tree["/bin/ls"].blake3.as_ref().unwrap(),
            tree["/bin/busybox"].blake3.as_ref().unwrap()
        );
    }

    #[test]
    fn file_replacing_dir_shadows_subtree() {
        let (_d, store) = temp_store();
        let mut tree = BTreeMap::new();
        apply(
            &store,
            &mut tree,
            build_tar(vec![
                ("app", tar::EntryType::Directory, b"", None),
                ("app/f", tar::EntryType::Regular, b"f", None),
            ]),
        );
        apply(
            &store,
            &mut tree,
            build_tar(vec![("app", tar::EntryType::Regular, b"now a file", None)]),
        );
        assert_eq!(tree["/app"].kind, EntryKind::File);
        assert!(!tree.contains_key("/app/f"));
    }
}
