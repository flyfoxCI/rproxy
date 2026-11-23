use anyhow::{bail, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Read, path::Path};
use tempfile::TempDir;
use xtask::release::{
    build_archive, package_with_gate, verify_archives, write_checksums, NativeGate, TARGETS,
};

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("docs")).unwrap();
    for (name, content) in [
        ("Cargo.toml", "[package]\nversion=\"0.2.4\"\n"),
        ("README.md", "usage"),
        ("docs/rproxy-cli.md", "CLI"),
        ("docs/internal-collector.md", "collector"),
        ("rproxy.example.toml", "example"),
        ("sources.example.toml", "sources=[]"),
        ("providers.env.example", "# provider"),
        (".env", "PRIVATE"),
        ("rproxy.toml", "PRIVATE"),
        ("master.key", "PRIVATE"),
    ] {
        fs::write(root.join(name), content).unwrap();
    }
    for target in TARGETS {
        let bin = root.join("target").join(target).join("release");
        fs::create_dir_all(&bin).unwrap();
        for name in ["rproxy", "rproxy-cli"] {
            fs::write(
                bin.join(format!(
                    "{name}{}",
                    if target.contains("windows") {
                        ".exe"
                    } else {
                        ""
                    }
                )),
                b"FAKE INPUT NEVER EXECUTE",
            )
            .unwrap();
        }
    }
    dir
}

fn contents(path: &Path) -> BTreeMap<String, (u32, Vec<u8>)> {
    let file = fs::File::open(path).unwrap();
    let mut result = BTreeMap::new();
    if path.extension().unwrap() == "zip" {
        let mut zip = zip::ZipArchive::new(file).unwrap();
        for n in 0..zip.len() {
            let mut item = zip.by_index(n).unwrap();
            let mut data = Vec::new();
            item.read_to_end(&mut data).unwrap();
            result.insert(
                item.name().to_owned(),
                (item.unix_mode().unwrap() & 0o777, data),
            );
        }
    } else {
        let mut tar = tar::Archive::new(GzDecoder::new(file));
        for item in tar.entries().unwrap() {
            let mut item = item.unwrap();
            let mut data = Vec::new();
            item.read_to_end(&mut data).unwrap();
            result.insert(
                item.path().unwrap().to_str().unwrap().to_owned(),
                (item.header().mode().unwrap(), data),
            );
        }
    }
    result
}

#[test]
fn archives_contain_only_public_allowlist_with_normalized_modes() {
    let fixture = fixture();
    let root = fixture.path();
    for target in TARGETS {
        let path = build_archive(root, target, "v0.2.4", &root.join("dist")).unwrap();
        let prefix = format!("rproxy-v0.2.4-{target}/");
        let items = contents(&path);
        let exe = if target.contains("windows") {
            ".exe"
        } else {
            ""
        };
        let mut names: Vec<String> = [
            "README.md",
            "docs/rproxy-cli.md",
            "docs/internal-collector.md",
            "rproxy.example.toml",
            "sources.example.toml",
            "providers.env.example",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        names.extend([format!("rproxy{exe}"), format!("rproxy-cli{exe}")]);
        assert_eq!(items.len(), 8);
        for name in names {
            let (mode, data) = &items[&format!("{prefix}{name}")];
            let binary = name == format!("rproxy{exe}") || name == format!("rproxy-cli{exe}");
            assert_eq!(*mode, if binary { 0o755 } else { 0o644 });
            if binary {
                assert_eq!(data, b"FAKE INPUT NEVER EXECUTE");
            }
        }
    }
}

#[test]
fn publication_requires_exact_five_archives_and_rejects_old_assets() {
    let fixture = fixture();
    let root = fixture.path();
    let dist = root.join("dist");
    for target in TARGETS {
        assert!(verify_archives(&dist, "v0.2.4").is_err());
        build_archive(root, target, "v0.2.4", &dist).unwrap();
    }
    assert_eq!(verify_archives(&dist, "v0.2.4").unwrap().len(), 5);
    write_checksums(&dist).unwrap();
    assert_eq!(verify_archives(&dist, "v0.2.4").unwrap().len(), 5);
    fs::write(dist.join("rproxy-v0.2.3-x86_64-apple-darwin.tar.gz"), "old").unwrap();
    assert!(verify_archives(&dist, "v0.2.4").is_err());
}

#[test]
fn malformed_tags_targets_missing_empty_or_linked_inputs_cannot_publish() {
    let fixture = fixture();
    let root = fixture.path();
    let dist = root.join("dist");
    for tag in ["v0.3.0", "../v0.2.4", "v0.2.4;bad", "v00.2.4", "0.2.4"] {
        assert!(build_archive(root, TARGETS[0], tag, &dist).is_err());
    }
    assert!(build_archive(root, "x86_64-unknown-linux-gnu", "v0.2.4", &dist).is_err());
    let cli = root
        .join("target")
        .join(TARGETS[0])
        .join("release/rproxy-cli");
    fs::write(&cli, "").unwrap();
    assert!(build_archive(root, TARGETS[0], "v0.2.4", &dist).is_err());
    fs::remove_file(&cli).unwrap();
    assert!(build_archive(root, TARGETS[0], "v0.2.4", &dist).is_err());
    assert!(!dist.exists());
}

struct Gate {
    called: bool,
    fail: bool,
}
impl NativeGate for Gate {
    fn check(&mut self, server: &Path, cli: &Path, version: &str) -> Result<()> {
        assert_eq!(server.file_name().unwrap(), "rproxy.exe");
        assert_eq!(cli.file_name().unwrap(), "rproxy-cli.exe");
        assert_eq!(version, "0.2.4");
        self.called = true;
        if self.fail {
            bail!("SQLite database is locked; exit code 1");
        }
        Ok(())
    }
}
#[test]
fn packaging_calls_native_gate_before_writing_archive_and_preserves_failure_cause() {
    let fixture = fixture();
    let root = fixture.path();
    let dist = root.join("dist");
    let mut gate = Gate {
        called: false,
        fail: true,
    };
    let error =
        package_with_gate(root, "x86_64-pc-windows-msvc", "v0.2.4", &dist, &mut gate).unwrap_err();
    assert!(gate.called);
    assert!(format!("{error:#}").contains("database is locked"));
    assert!(!dist.exists());
    gate.fail = false;
    assert!(
        package_with_gate(root, "x86_64-pc-windows-msvc", "v0.2.4", &dist, &mut gate)
            .unwrap()
            .is_file()
    );
}

#[test]
fn checksums_are_sorted_safe_and_match_archive_bytes() {
    let fixture = fixture();
    let root = fixture.path();
    let dist = root.join("dist");
    assert!(write_checksums(root).is_err());
    let archive = build_archive(root, TARGETS[0], "v0.2.4", &dist).unwrap();
    let sums = write_checksums(&dist).unwrap();
    let expected = format!(
        "{:x}  {}\n",
        Sha256::digest(fs::read(&archive).unwrap()),
        archive.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(fs::read_to_string(&sums).unwrap(), expected);
    fs::write(dist.join("unexpected.zip"), "bad").unwrap();
    assert!(write_checksums(&dist).is_err());
    #[cfg(unix)]
    {
        fs::remove_file(dist.join("unexpected.zip")).unwrap();
        fs::write(dist.join("bad\nname.zip"), "bad").unwrap();
        assert!(write_checksums(&dist).is_err());
    }
}

#[test]
fn verifier_rejects_missing_empty_symlink_nonexec_and_extra_archive_members() {
    for bad in [
        "missing",
        "empty",
        "link",
        "mode",
        "extra",
        "traversal",
        "duplicate",
    ] {
        let fixture = fixture();
        let root = fixture.path();
        let dist = root.join("dist");
        for target in TARGETS {
            build_archive(root, target, "v0.2.4", &dist).unwrap();
        }
        let path = dist.join("rproxy-v0.2.4-aarch64-apple-darwin.tar.gz");
        let original = contents(&path);
        let prefix = "rproxy-v0.2.4-aarch64-apple-darwin/";
        let file = fs::File::create(&path).unwrap();
        let mut bundle = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        ));
        for (name, (mode, data)) in original {
            if bad == "missing" && name.ends_with("/rproxy-cli") {
                continue;
            }
            let is_cli = name.ends_with("/rproxy-cli");
            let data = if bad == "empty" && is_cli {
                Vec::new()
            } else {
                data
            };
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(if bad == "mode" && is_cli { 0o644 } else { mode });
            if bad == "link" && is_cli {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name("rproxy").unwrap();
                header.set_size(0);
            }
            header.set_cksum();
            bundle.append_data(&mut header, &name, &data[..]).unwrap();
            if bad == "duplicate" && is_cli {
                bundle.append_data(&mut header, &name, &data[..]).unwrap();
            }
        }
        if bad == "extra" || bad == "traversal" {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            let name = if bad == "extra" {
                format!("{prefix}master.key")
            } else {
                "elsewhere/private".to_owned()
            };
            bundle.append_data(&mut header, name, &b"x"[..]).unwrap();
        }
        bundle.into_inner().unwrap().finish().unwrap();
        assert!(verify_archives(&dist, "v0.2.4").is_err(), "accepted {bad}");
    }
}

#[test]
fn zip_verification_rejects_missing_empty_linked_duplicate_and_unsafe_members() {
    use std::io::Write;
    for bad in [
        "missing",
        "empty",
        "link",
        "directory",
        "mode",
        "duplicate",
        "traversal",
        "absolute",
        "backslash",
    ] {
        let fixture = fixture();
        let root = fixture.path();
        let dist = root.join("dist");
        for target in TARGETS {
            build_archive(root, target, "v0.2.4", &dist).unwrap();
        }
        let path = dist.join("rproxy-v0.2.4-x86_64-pc-windows-msvc.zip");
        let original = contents(&path);
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        for (name, (mode, data)) in original {
            let cli = name.ends_with("/rproxy-cli.exe");
            if cli && bad == "missing" {
                continue;
            }
            let options = zip::write::SimpleFileOptions::default()
                .unix_permissions(if cli && bad == "mode" { 0o644 } else { mode });
            if cli && bad == "link" {
                zip.add_symlink(&name, "rproxy.exe", options).unwrap();
                continue;
            }
            if cli && bad == "directory" {
                zip.add_directory(&name, options).unwrap();
                continue;
            }
            zip.start_file(&name, options).unwrap();
            if !(cli && bad == "empty") {
                zip.write_all(&data).unwrap();
            }
            // zip's writer rejects duplicates itself; mutate a same-length name below instead.
        }
        match bad {
            "traversal" => {
                zip.start_file("../private", zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"x").unwrap();
            }
            "absolute" => {
                zip.start_file("/private", zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"x").unwrap();
            }
            "backslash" => {
                zip.start_file("..\\private", zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"x").unwrap();
            }
            "duplicate" => {
                let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
                zip.start_file(
                    "rproxy-v0.2.4-x86_64-pc-windows-msvc/rproxy-new.exe",
                    options,
                )
                .unwrap();
                zip.write_all(b"duplicate input").unwrap();
            }
            _ => {}
        }
        zip.finish().unwrap();
        if bad == "duplicate" {
            let mut bytes = fs::read(&path).unwrap();
            for index in 0..bytes.len().saturating_sub(14) {
                if &bytes[index..index + 14] == b"rproxy-new.exe" {
                    bytes[index..index + 14].copy_from_slice(b"rproxy-cli.exe");
                }
            }
            fs::write(&path, bytes).unwrap();
        }
        assert!(
            verify_archives(&dist, "v0.2.4").is_err(),
            "accepted ZIP {bad}"
        );
    }
}

#[test]
fn verifier_rejects_mismatched_checksum_manifest_without_executing_binaries() {
    let fixture = fixture();
    let root = fixture.path();
    let dist = root.join("dist");
    for target in TARGETS {
        build_archive(root, target, "v0.2.4", &dist).unwrap();
    }
    write_checksums(&dist).unwrap();
    let sums = dist.join("SHA256SUMS");
    let original = fs::read_to_string(&sums).unwrap();
    fs::write(
        &sums,
        original.replacen(&original[..64], &"0".repeat(64), 1),
    )
    .unwrap();
    assert!(verify_archives(&dist, "v0.2.4").is_err());
}

#[test]
fn invalid_cargo_version_metadata_returns_error_without_panicking_or_publishing() {
    let fixture = fixture();
    let root = fixture.path();
    fs::write(root.join("Cargo.toml"), "[package]\nname=\"rproxy\"\n").unwrap();
    assert!(build_archive(root, TARGETS[0], "v0.2.4", &root.join("dist")).is_err());
    assert!(!root.join("dist").exists());
}

#[cfg(unix)]
#[test]
fn packaging_and_checksums_reject_symlink_files() {
    use std::os::unix::fs::symlink;
    let fixture = fixture();
    let root = fixture.path();
    let dist = root.join("dist");
    let binary = root
        .join("target")
        .join(TARGETS[0])
        .join("release/rproxy-cli");
    fs::remove_file(&binary).unwrap();
    symlink(root.join("README.md"), &binary).unwrap();
    assert!(build_archive(root, TARGETS[0], "v0.2.4", &dist).is_err());
    build_archive(root, TARGETS[1], "v0.2.4", &dist).unwrap();
    symlink(root.join("README.md"), dist.join("SHA256SUMS")).unwrap();
    assert!(write_checksums(&dist).is_err());
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), "usage");
}
