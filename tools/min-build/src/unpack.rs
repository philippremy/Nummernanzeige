//! Unpacking downloaded archives into `.tools`.

use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
};

use tar::EntryType;
use xz_native::XzReader;
use zip::ZipArchive;

use crate::{error::InstallError, progress::ComponentBar};

/// Unpacks a zip archive held in memory into `target`, leaving out the
/// archive's top folder (llvm-mingw puts everything into
/// `llvm-mingw-<version>-ucrt-x86_64/`).
///
/// Stage 2 of `bar` counts the unpacked bytes written. Files already in
/// `target` are overwritten, so a run that broke off can simply be repeated.
pub fn unpack_zip(
    archive_name: &str,
    data: Vec<u8>,
    target: &Path,
    bar: &ComponentBar,
) -> Result<(), InstallError> {
    let broken = |source: zip::result::ZipError| InstallError::unpack(archive_name, source);

    let mut archive = ZipArchive::new(Cursor::new(data)).map_err(broken)?;

    // The table of contents lists every file's unpacked size; reading it does
    // not unpack anything yet.
    let mut total = 0;
    for index in 0..archive.len() {
        total += archive.by_index_raw(index).map_err(broken)?.size();
    }
    bar.start_unpack(total);

    let mut buffer = vec![0; 256 * 1024];
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(broken)?;

        // `enclosed_name` refuses paths that would end up outside `target`
        // (absolute paths, `..`), so a tampered archive cannot write elsewhere.
        let Some(name) = entry.enclosed_name() else {
            let message = format!("unzulässiger Pfad im Archiv: {:?}", entry.name());
            return Err(InstallError::unpack(archive_name, message));
        };
        let relative: PathBuf = name.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue; // the top folder itself
        }
        let path = target.join(relative);

        if entry.is_dir() {
            create_dir(&path)?;
            continue;
        }
        if entry.is_symlink() {
            let message = format!("unerwarteter Link im Archiv: {}", name.display());
            return Err(InstallError::unpack(archive_name, message));
        }

        write_file(archive_name, &mut entry, &path, &mut buffer, |n| bar.inc(n))?;
    }

    Ok(())
}

/// Unpacks one Rust component archive (`.tar.xz`, held in memory) into `target`.
///
/// Rust's archives look like `rustc-1.100.0-<target>/rustc/bin/rustc.exe`, next
/// to installer files (`install.sh`, licences, …). Only what is inside the
/// folder `component` is unpacked, without the two leading folder names, so
/// `rustc.exe` ends up in `target/bin/`. The component's `manifest.in`
/// (the installer's file list) is left out too.
///
/// Progress counts the compressed bytes read, so call `bar.start_unpack`
/// beforehand with the archives' total size.
pub fn unpack_rust_component(
    archive_name: &str,
    data: &[u8],
    component: &str,
    target: &Path,
    bar: &ComponentBar,
) -> Result<(), InstallError> {
    let broken = |source: std::io::Error| InstallError::unpack(archive_name, source);

    let compressed = BufReader::new(bar.wrap_read(Cursor::new(data)));
    let mut archive = tar::Archive::new(XzReader::new(compressed));

    let mut buffer = vec![0; 256 * 1024];
    let mut found_component = false;
    for entry in archive.entries().map_err(broken)? {
        let mut entry = entry.map_err(broken)?;
        let name = entry.path().map_err(broken)?.into_owned();

        // Only plain names; `..`, absolute paths or drive letters could
        // otherwise make a tampered archive write outside `target`.
        if !name
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            let message = format!("unzulässiger Pfad im Archiv: {}", name.display());
            return Err(InstallError::unpack(archive_name, message));
        }
        let mut parts = name.components();
        parts.next(); // e.g. `rustc-1.100.0-x86_64-pc-windows-gnullvm`
        if parts.next().map(|part| part.as_os_str()) != Some(component.as_ref()) {
            continue; // installer files next to the component
        }
        found_component = true;
        let relative = parts.as_path();
        if relative.as_os_str().is_empty() || relative == Path::new("manifest.in") {
            continue;
        }
        let path = target.join(relative);

        match entry.header().entry_type() {
            EntryType::Directory => create_dir(&path)?,
            EntryType::Regular | EntryType::Continuous => {
                write_file(archive_name, &mut entry, &path, &mut buffer, |_| {})?
            }
            other => {
                let message = format!("unerwarteter Eintrag ({other:?}): {}", name.display());
                return Err(InstallError::unpack(archive_name, message));
            }
        }
    }

    if !found_component {
        let message = format!("Ordner {component:?} fehlt im Archiv");
        return Err(InstallError::unpack(archive_name, message));
    }
    Ok(())
}

/// Writes one file from an archive to `path`, creating its folder if needed.
/// `progress` is called with each number of bytes written.
///
/// Copied by hand instead of `io::copy`, so that a broken archive (read
/// error) and a full disk (write error) are reported as what they are.
fn write_file(
    archive_name: &str,
    source: &mut impl Read,
    path: &Path,
    buffer: &mut [u8],
    mut progress: impl FnMut(u64),
) -> Result<(), InstallError> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let file = File::create(path).map_err(|e| InstallError::fs("erstellen", path, e))?;
    let mut out = BufWriter::new(file);
    loop {
        let read = source
            .read(buffer)
            .map_err(|e| InstallError::unpack(archive_name, e))?;
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read])
            .map_err(|e| InstallError::fs("schreiben", path, e))?;
        progress(read as u64);
    }
    out.flush()
        .map_err(|e| InstallError::fs("schreiben", path, e))
}

/// Marks an installation as complete by writing `marker`, as the very last step.
/// Its presence is what tells the next start that the tool is there completely,
/// rather than half unpacked from a run that broke off.
pub fn write_marker(marker: &Path, content: &str) -> Result<(), InstallError> {
    fs::write(marker, content).map_err(|e| InstallError::fs("schreiben", marker, e))
}

fn create_dir(path: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(path).map_err(|e| InstallError::fs("anlegen", path, e))
}
