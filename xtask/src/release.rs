use anyhow::{bail, ensure, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use zip::{write::SimpleFileOptions, CompressionMethod};

pub const TARGETS: [&str; 5] = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
];
const PUBLIC_FILES: [&str; 6] = [
    "README.md",
    "docs/rproxy-cli.md",
    "docs/internal-collector.md",
    "rproxy.example.toml",
    "sources.example.toml",
    "providers.env.example",
];

pub trait NativeGate {
    fn check(&mut self, server: &Path, cli: &Path, version: &str) -> Result<()>;
}

pub fn validate_tag(tag: &str) -> Result<()> {
    let parts: Vec<_> = tag.strip_prefix('v').unwrap_or("").split('.').collect();
    ensure!(
        parts.len() == 3
            && parts.iter().all(|s| !s.is_empty()
                && s.bytes().all(|c| c.is_ascii_digit())
                && (s.len() == 1 || !s.starts_with('0'))),
        "invalid release tag"
    );
    Ok(())
}

fn validate(root: &Path, target: &str, tag: &str) -> Result<String> {
    ensure!(
        TARGETS.contains(&target),
        "unsupported native release target"
    );
    validate_tag(tag)?;
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(root.join("Cargo.toml"))?)?;
    let version = manifest
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .context("missing Cargo package version")?;
    ensure!(
        tag == format!("v{version}"),
        "release tag must match Cargo.toml version"
    );
    Ok(version.to_owned())
}

pub fn regular_file(path: &Path, nonempty: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing release file: {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file() && (!nonempty || metadata.len() > 0),
        "release input must be a {}regular file: {}",
        if nonempty { "nonempty " } else { "" },
        path.display()
    );
    Ok(())
}

fn directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "release directory must be a real directory"
    );
    Ok(())
}

fn members(target: &str) -> Vec<(String, u32)> {
    let suffix = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let mut files = vec![
        (format!("rproxy{suffix}"), 0o755),
        (format!("rproxy-cli{suffix}"), 0o755),
    ];
    files.extend(PUBLIC_FILES.iter().map(|p| ((*p).to_owned(), 0o644)));
    files
}

fn inputs(root: &Path, target: &str) -> Result<Vec<(PathBuf, String, u32)>> {
    let files: Vec<_> = members(target)
        .into_iter()
        .map(|(relative, mode)| {
            let path = if mode == 0o755 {
                root.join("target")
                    .join(target)
                    .join("release")
                    .join(&relative)
            } else {
                root.join(&relative)
            };
            (path, relative, mode)
        })
        .collect();
    for (path, _, mode) in &files {
        regular_file(path, *mode == 0o755)?;
    }
    Ok(files)
}

fn filename(target: &str, tag: &str) -> String {
    format!(
        "rproxy-{tag}-{target}{}",
        if target.contains("windows") {
            ".zip"
        } else {
            ".tar.gz"
        }
    )
}

fn safe_destination(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => regular_file(path, false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Pure archive construction: never executes inputs, including test fixtures.
pub fn build_archive(root: &Path, target: &str, tag: &str, output: &Path) -> Result<PathBuf> {
    validate(root, target, tag)?;
    let files = inputs(root, target)?;
    fs::create_dir_all(output)?;
    directory(output)?;
    let destination = output.join(filename(target, tag));
    safe_destination(&destination)?;
    let prefix = format!("rproxy-{tag}-{target}");
    let mut temporary = tempfile::NamedTempFile::new_in(output)?;
    if target.contains("windows") {
        let mut archive = zip::ZipWriter::new(temporary.as_file_mut());
        for (path, relative, mode) in files {
            archive.start_file(
                format!("{prefix}/{relative}"),
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .unix_permissions(mode),
            )?;
            io::copy(&mut File::open(path)?, &mut archive)?;
        }
        archive.finish()?;
    } else {
        let mut archive = tar::Builder::new(GzEncoder::new(
            temporary.as_file_mut(),
            Compression::default(),
        ));
        for (path, relative, mode) in files {
            let mut source = File::open(path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(source.metadata()?.len());
            header.set_mode(mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            archive.append_data(&mut header, format!("{prefix}/{relative}"), &mut source)?;
        }
        archive.into_inner()?.finish()?;
    }
    temporary.persist(&destination)?;
    Ok(destination)
}

pub fn package_with_gate(
    root: &Path,
    target: &str,
    tag: &str,
    output: &Path,
    gate: &mut impl NativeGate,
) -> Result<PathBuf> {
    let version = validate(root, target, tag)?;
    let files = inputs(root, target)?;
    gate.check(&files[0].0, &files[1].0, &version)?;
    build_archive(root, target, tag, output)
}

fn check_member(
    name: &str,
    mode: u32,
    size: u64,
    regular: bool,
    expected: &[(String, u32)],
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    let (_, want_mode) = expected
        .iter()
        .find(|(path, _)| path == name)
        .context("unsafe or non-allowlisted archive member")?;
    ensure!(seen.insert(name.to_owned()), "duplicate archive member");
    ensure!(regular, "archive members must be regular files");
    ensure!(
        mode & 0o7777 == *want_mode,
        "archive member has unsafe or non-executable mode"
    );
    ensure!(
        *want_mode != 0o755 || size > 0,
        "archive must contain both nonempty binaries"
    );
    Ok(())
}

pub fn verify_archives(directory_path: &Path, tag: &str) -> Result<Vec<PathBuf>> {
    validate_tag(tag)?;
    directory(directory_path)?;
    let expected: BTreeSet<_> = TARGETS.iter().map(|t| filename(t, tag)).collect();
    let actual: BTreeSet<_> = fs::read_dir(directory_path)?
        .map(|e| {
            e?.file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("unsafe release filename"))
        })
        .collect::<Result<_>>()?;
    let mut wanted = expected.clone();
    if actual.contains("SHA256SUMS") {
        regular_file(&directory_path.join("SHA256SUMS"), true)?;
        wanted.insert("SHA256SUMS".to_owned());
    }
    ensure!(
        actual == wanted,
        "incomplete or unexpected release archives"
    );
    for target in TARGETS {
        let path = directory_path.join(filename(target, tag));
        regular_file(&path, true)?;
        let prefix = format!("rproxy-{tag}-{target}/");
        let expected: Vec<_> = members(target)
            .into_iter()
            .map(|(p, m)| (format!("{prefix}{p}"), m))
            .collect();
        let mut seen = BTreeSet::new();
        if target.contains("windows") {
            let mut archive = zip::ZipArchive::new(File::open(&path)?)?;
            check_zip_directory(&path, archive.central_directory_start(), archive.len())?;
            for index in 0..archive.len() {
                let mut member = archive.by_index(index)?;
                let mode = member
                    .unix_mode()
                    .context("archive member lacks regular file mode")?;
                check_member(
                    member.name(),
                    mode,
                    member.size(),
                    mode & 0o170000 == 0o100000 && !member.is_dir(),
                    &expected,
                    &mut seen,
                )?;
                io::copy(&mut member, &mut io::sink())?;
            }
        } else {
            let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
            for member in archive.entries()? {
                let mut member = member?;
                let name = member
                    .path()?
                    .to_str()
                    .context("non-UTF8 archive member")?
                    .to_owned();
                check_member(
                    &name,
                    member.header().mode()?,
                    member.size(),
                    member.header().entry_type().is_file(),
                    &expected,
                    &mut seen,
                )?;
                io::copy(&mut member, &mut io::sink())?;
            }
        }
        ensure!(
            seen.len() == expected.len(),
            "archive must contain both binaries and all public files"
        );
    }
    let archives: Vec<_> = expected
        .into_iter()
        .map(|p| directory_path.join(p))
        .collect();
    if actual.contains("SHA256SUMS") {
        let path = directory_path.join("SHA256SUMS");
        ensure!(
            fs::metadata(&path)?.len() <= 4096,
            "checksum manifest too large"
        );
        ensure!(
            fs::read_to_string(path)? == checksum_lines(&archives)?,
            "SHA256SUMS does not match release archives"
        );
    }
    Ok(archives)
}

// zip 2 indexes central entries by name, silently collapsing duplicates. Check the
// raw record count too, so a duplicate cannot hide from by_index/len inspection.
fn check_zip_directory(path: &Path, offset: u64, unique_count: usize) -> Result<()> {
    let mut input = File::open(path)?;
    input.seek(SeekFrom::Start(offset))?;
    for count in 0..=unique_count {
        let mut signature = [0; 4];
        input.read_exact(&mut signature)?;
        if signature == *b"PK\x05\x06" || signature == *b"PK\x06\x06" {
            ensure!(
                count == unique_count,
                "ZIP central directory member count mismatch"
            );
            return Ok(());
        }
        ensure!(signature == *b"PK\x01\x02", "invalid ZIP central directory");
        ensure!(
            count < unique_count,
            "duplicate ZIP central directory member"
        );
        let mut record = [0; 42];
        input.read_exact(&mut record)?;
        let variable_size: i64 = [24, 26, 28]
            .into_iter()
            .map(|index| i64::from(u16::from_le_bytes([record[index], record[index + 1]])))
            .sum();
        input.seek(SeekFrom::Current(variable_size))?;
    }
    bail!("invalid ZIP central directory")
}

fn safe_archive_name(name: &str) -> Result<()> {
    for target in TARGETS {
        let suffix = format!(
            "-{target}{}",
            if target.contains("windows") {
                ".zip"
            } else {
                ".tar.gz"
            }
        );
        if let Some(tag) = name
            .strip_prefix("rproxy-")
            .and_then(|n| n.strip_suffix(&suffix))
        {
            return validate_tag(tag);
        }
    }
    bail!("unsafe or unexpected checksum input filename")
}

pub fn write_checksums(directory_path: &Path) -> Result<PathBuf> {
    directory(directory_path)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory_path)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|p| p.to_str())
            .context("unsafe checksum input filename")?;
        if name == "SHA256SUMS" {
            regular_file(&path, false)?;
            continue;
        }
        safe_archive_name(name)?;
        regular_file(&path, true)?;
        paths.push(path);
    }
    paths.sort();
    ensure!(!paths.is_empty(), "no release archives to checksum");
    let destination = directory_path.join("SHA256SUMS");
    safe_destination(&destination)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory_path)?;
    temporary.write_all(checksum_lines(&paths)?.as_bytes())?;
    temporary.persist(&destination)?;
    Ok(destination)
}

fn checksum_lines(paths: &[PathBuf]) -> Result<String> {
    let mut lines = String::new();
    for path in paths {
        let mut hash = Sha256::new();
        let mut input = File::open(path)?;
        let mut buffer = [0; 65536];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        use std::fmt::Write;
        writeln!(
            lines,
            "{:x}  {}",
            hash.finalize(),
            path.file_name().unwrap().to_str().unwrap()
        )?;
    }
    Ok(lines)
}
