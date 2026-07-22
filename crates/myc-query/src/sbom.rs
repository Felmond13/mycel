//! SBOM generation straight from the manifest.
//!
//! The manifest already *is* a complete file inventory with content hashes.
//! On top of that, when the environment ships a package database (Alpine's
//! apk or Debian's dpkg) we parse the real installed-package list out of the
//! blob content — no guessing, no scanner.

use myc_manifest::Manifest;
use myc_store::Store;
use serde::Serialize;

/// Package databases larger than this are ignored (defensive cap).
const MAX_DB_BYTES: u64 = 64 * 1024 * 1024;

const APK_DB_PATH: &str = "/lib/apk/db/installed";
const DPKG_DB_PATH: &str = "/var/lib/dpkg/status";

#[derive(Debug, Clone, Serialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// `apk` or `dpkg`.
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SbomFile {
    pub path: String,
    pub blake3: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct Sbom {
    pub format: &'static str,
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub os: String,
    pub arch: String,
    pub created: String,
    /// Which package database was found, if any (`apk`, `dpkg`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_source: Option<String>,
    pub packages: Vec<Package>,
    pub file_count: usize,
    pub logical_size: u64,
    pub files: Vec<SbomFile>,
}

/// Parse Alpine's `/lib/apk/db/installed` format: records separated by blank
/// lines, single-letter keys (`P:` name, `V:` version, `A:` arch).
pub fn parse_apk_installed(text: &str) -> Vec<Package> {
    let mut packages = Vec::new();
    for record in text.split("\n\n") {
        let mut name = None;
        let mut version = None;
        let mut arch = None;
        for line in record.lines() {
            match line.split_once(':') {
                Some(("P", v)) => name = Some(v.trim().to_string()),
                Some(("V", v)) => version = Some(v.trim().to_string()),
                Some(("A", v)) => arch = Some(v.trim().to_string()),
                _ => {}
            }
        }
        if let (Some(name), Some(version)) = (name, version) {
            packages.push(Package {
                name,
                version,
                architecture: arch,
                source: "apk".to_string(),
            });
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

/// Parse Debian's `/var/lib/dpkg/status`: RFC-822-style stanzas; only
/// packages whose `Status` ends in `installed` are included.
pub fn parse_dpkg_status(text: &str) -> Vec<Package> {
    let mut packages = Vec::new();
    for stanza in text.split("\n\n") {
        let mut name = None;
        let mut version = None;
        let mut arch = None;
        let mut installed = false;
        for line in stanza.lines() {
            match line.split_once(": ") {
                Some(("Package", v)) => name = Some(v.trim().to_string()),
                Some(("Version", v)) => version = Some(v.trim().to_string()),
                Some(("Architecture", v)) => arch = Some(v.trim().to_string()),
                Some(("Status", v)) => installed = v.trim().ends_with("installed"),
                _ => {}
            }
        }
        if installed {
            if let (Some(name), Some(version)) = (name, version) {
                packages.push(Package {
                    name,
                    version,
                    architecture: arch,
                    source: "dpkg".to_string(),
                });
            }
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

fn read_db_blob(store: &Store, manifest: &Manifest, path: &str) -> Option<String> {
    let entry = manifest.entries.iter().find(|e| e.path == path)?;
    if entry.size > MAX_DB_BYTES {
        return None;
    }
    let hash = entry.blake3.as_deref()?;
    let blob_path = store.blob(hash).ok()?;
    std::fs::read_to_string(blob_path).ok()
}

/// Build the SBOM for `manifest` (store id `id`), reading package databases
/// out of the content store when the environment ships one.
pub fn sbom(store: &Store, id: &str, manifest: &Manifest) -> Sbom {
    let (package_source, packages) = if let Some(text) = read_db_blob(store, manifest, APK_DB_PATH)
    {
        (Some("apk".to_string()), parse_apk_installed(&text))
    } else if let Some(text) = read_db_blob(store, manifest, DPKG_DB_PATH) {
        (Some("dpkg".to_string()), parse_dpkg_status(&text))
    } else {
        (None, Vec::new())
    };

    let files: Vec<SbomFile> = manifest
        .entries
        .iter()
        .filter_map(|e| {
            e.blake3.as_ref().map(|h| SbomFile {
                path: e.path.clone(),
                blake3: h.clone(),
                size: e.size,
            })
        })
        .collect();

    Sbom {
        format: "mycel-sbom",
        schema: 1,
        id: id.to_string(),
        name: manifest.name.clone(),
        os: manifest.os.clone(),
        arch: manifest.arch.clone(),
        created: manifest.created.clone(),
        package_source,
        packages,
        file_count: files.len(),
        logical_size: manifest.logical_size(),
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_apk_installed_records() {
        let db = "C:Q1abcdef=\nP:musl\nV:1.2.5-r0\nA:x86_64\nT:the musl c library\n\n\
                  C:Q2ghijkl=\nP:busybox\nV:1.36.1-r29\nA:x86_64\n\n\
                  P:incomplete-no-version\n";
        let pkgs = parse_apk_installed(db);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "busybox");
        assert_eq!(pkgs[0].version, "1.36.1-r29");
        assert_eq!(pkgs[0].architecture.as_deref(), Some("x86_64"));
        assert_eq!(pkgs[0].source, "apk");
        assert_eq!(pkgs[1].name, "musl");
        assert_eq!(pkgs[1].version, "1.2.5-r0");
    }

    #[test]
    fn parses_dpkg_status_installed_only() {
        let db = "Package: libc6\nStatus: install ok installed\nVersion: 2.36-9+deb12u4\nArchitecture: amd64\n\n\
                  Package: removed-pkg\nStatus: deinstall ok config-files\nVersion: 1.0\nArchitecture: amd64\n\n\
                  Package: bash\nStatus: install ok installed\nVersion: 5.2.15-2+b2\nArchitecture: amd64\n";
        let pkgs = parse_dpkg_status(db);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "bash");
        assert_eq!(pkgs[1].name, "libc6");
        assert_eq!(pkgs[1].version, "2.36-9+deb12u4");
        assert_eq!(pkgs[1].source, "dpkg");
    }

    #[test]
    fn empty_input_yields_no_packages() {
        assert!(parse_apk_installed("").is_empty());
        assert!(parse_dpkg_status("").is_empty());
    }
}
