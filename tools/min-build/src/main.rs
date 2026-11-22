use std::{
    io::Read,
    path::PathBuf,
    sync::{LazyLock, OnceLock},
    thread::JoinHandle,
};

use console::{Term, style};
use dialoguer::Confirm;
use hex_literal::hex;

use crate::error::{InstallError, verify_sha256};
use crate::progress::{BuildBar, ComponentBar, Progress};

mod compile;
mod error;
mod progress;
mod unpack;

const MINGW_DOWNLOAD_FILE: &str = "https://github.com/mstorsjo/llvm-mingw/releases/download/20261117/llvm-mingw-20261117-ucrt-x86_64.zip";
const MINGW_HASH: [u8; 32] =
    hex!("4d002e330a0f703625aa5ccbc37e96f8f824413e163bfc7336774aabd2c3a412");
// All paths below are relative to the `.tools` folder next to the executable;
// turn them into real paths with `tool_path`.
const MINGW_DIR: &str = "llvm-mingw";
const MINGW_MARKER: &str = "llvm-mingw/.installed";

/// Rust 1.100.0, the three packages a build needs, each as `.tar.xz`:
/// (name for messages, URL, SHA-256, folder inside the archive to unpack).
const RUST_PACKAGES: [(&str, &str, [u8; 32], &str); 3] = [
    (
        "rustc",
        "https://static.rust-lang.org/dist/2026-11-12/rustc-1.100.0-x86_64-pc-windows-gnullvm.tar.xz",
        hex!("1d4f93b7264c098bf90c86b48b7953a9009f7dc01e4c2bdc8ba30d9407ed47d9"),
        "rustc",
    ),
    (
        "rust-std",
        "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-x86_64-pc-windows-gnullvm.tar.xz",
        hex!("5e63b95c73bd783864b4350792c27292d67aab8e3033bf49381d0e55ef4384cf"),
        "rust-std-x86_64-pc-windows-gnullvm",
    ),
    (
        "cargo",
        "https://static.rust-lang.org/dist/2026-11-12/cargo-1.100.0-x86_64-pc-windows-gnullvm.tar.xz",
        hex!("626445dd78b4f8bf2a06862a47b8ec26ba211de52fd4c69f4c770536f2f6b9cc"),
        "cargo",
    ),
];
const RUST_DIR: &str = "";
const RUST_MARKER: &str = ".rust-installed";

/// For all downloads. HTTPS goes through the operating system (`native-tls`,
/// i.e. SChannel on Windows), trusting the certificates the system trusts.
/// Without `root_certs` set, ureq would panic: its default (a bundled copy of
/// Mozilla's list) is not compiled in.
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .build()
        .new_agent()
});

/// The `.tools` folder next to the executable; set once at the start of `main`.
static TOOLS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Finds the `.tools` folder: next to the running `.exe`, not in whatever
/// folder the program was started from.
fn locate_tools_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("das Programm liegt in keinem Ordner"))?;
    Ok(exe_dir.join(".tools"))
}

/// Turns a path relative to `.tools` (written with `/`) into a full path.
///
/// Joined part by part, so the path only uses `\` on Windows: a mix of `/` and
/// `\` is fine for ordinary paths, but not for the long `\\?\` form.
fn tool_path(relative: &str) -> PathBuf {
    let tools_dir = TOOLS_DIR
        .get()
        .expect("TOOLS_DIR is set at the start of main");
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(tools_dir.clone(), |path, part| path.join(part))
}

fn main() {
    // We want confirmation from the user
    let term = Term::stderr();
    term.clear_screen().unwrap();

    match locate_tools_dir() {
        Ok(tools_dir) => TOOLS_DIR.set(tools_dir).expect("main runs only once"),
        Err(error) => {
            term.write_line(&format!(
                "{} {error}",
                style("Konnte den Ordner dieses Programms nicht bestimmen:").red()
            ))
            .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            return;
        }
    }

    term.write_line(&format!(
        "Hallo {}, dieses kleine Programm hilft dir, deine Nummernanzeige neu zu kompilieren {}.\n",
        style("Uwe").blue(),
        style(":)").green()
    ))
    .unwrap();

    // We check if Rust tools are present
    const RUST_TOOLS_REQUIRED: &[&'static str] = &[RUST_MARKER, "bin/cargo.exe", "bin/rustc.exe"];
    let rust_is_present = RUST_TOOLS_REQUIRED
        .iter()
        .map(|relative| std::fs::exists(tool_path(relative)).unwrap())
        .all(|exists| exists == true);

    // We check if LLVM-MinGW tools are present
    // `x86_64-w64-mingw32-ld` is only a shell script in the Windows package;
    // the linker clang calls is `ld.lld.exe`. `windres` is needed by GPUI's build script.
    const MINGW_TOOLS_REQUIRED: &[&'static str] = &[
        MINGW_MARKER,
        "llvm-mingw/bin/x86_64-w64-mingw32-clang.exe",
        "llvm-mingw/bin/x86_64-w64-mingw32-windres.exe",
        "llvm-mingw/bin/ld.lld.exe",
    ];
    let mingw_is_present = MINGW_TOOLS_REQUIRED
        .iter()
        .map(|relative| std::fs::exists(tool_path(relative)).unwrap())
        .all(|exists| exists == true);

    term.write_line(&format!(
        "Die folgenden Tools werden benötigt (ein '{}' zeigt an, dass sie schon verfügbar sind):\n",
        style('*').green()
    ))
    .unwrap();
    term.write_line(&format!(
        "[{}] Rust + Cargo (Rust Toolchain)",
        style(if rust_is_present { '*' } else { ' ' }).green()
    ))
    .unwrap();
    term.write_line(&format!(
        "[{}] LLVM-MinGW (C Toolchain)",
        style(if mingw_is_present { '*' } else { ' ' }).green()
    ))
    .unwrap();
    term.write_line("").unwrap();

    if !mingw_is_present || !rust_is_present {
        term.write_line(&format!(
            "{}",
            style("Die fehlenden Tools müssen installiert werden.\n").yellow()
        ))
        .unwrap();

        let confirmed = Confirm::new()
            .with_prompt("Sollen die fehlenden Tools lokal (also nicht systemweit) verfügbar gemacht werden?")
            .default(false)
            .show_default(true)
            .wait_for_newline(true)
            .interact()
            .unwrap();

        if !confirmed {
            term.write_line(&format!(
                "Okay, dann nicht {}. Bis zum nächsten Mal!",
                style(":C").red()
            ))
            .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            return;
        }

        term.write_line("").unwrap();

        // Each thread ends its own progress line (`run` marks it done or failed)
        // and hands the error back, so the full messages can be printed below
        // the bars once both are done.
        let mut threads: Vec<(&str, JoinHandle<Result<(), InstallError>>)> = Vec::new();
        let progress = Progress::new();
        if !rust_is_present {
            let bar = progress.add("Rust + Cargo");
            threads.push((
                "Rust + Cargo",
                std::thread::spawn(move || bar.run(download_rust)),
            ));
        }
        if !mingw_is_present {
            let bar = progress.add("LLVM-MinGW");
            threads.push((
                "LLVM-MinGW",
                std::thread::spawn(move || bar.run(download_mingw)),
            ));
        }

        // Wait for every thread before printing anything: text printed while a
        // bar is still being drawn would garble the display.
        let results: Vec<_> = threads
            .into_iter()
            .map(|(name, join_handle)| (name, join_handle.join()))
            .collect();

        let mut errored = false;
        for (name, result) in results {
            let message = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error.to_string(),
                // A bug rather than a failure; the panic message is already on screen.
                Err(_) => "Das Programm ist abgestürzt.".to_owned(),
            };
            if !errored {
                term.write_line("").unwrap();
            }
            term.write_line(&format!(
                "{} {message}",
                style(format!("{name}:")).red().bold()
            ))
            .unwrap();
            errored = true;
        }

        if errored {
            std::thread::sleep(std::time::Duration::from_secs(3));
            return;
        }
    }

    build_nummernanzeige(&term);
}

/// Builds the project next to this program and waits for Enter before
/// returning, so the result stays readable when the window closes afterwards.
fn build_nummernanzeige(term: &Term) {
    let tools_dir = TOOLS_DIR
        .get()
        .expect("TOOLS_DIR is set at the start of main");
    let project = tools_dir
        .parent()
        .expect(".tools lies in the program's folder");
    let log = tools_dir.join("build.log");

    term.write_line(&format!(
        "{}\n",
        style("Die Nummernanzeige wird jetzt kompiliert. Beim ersten Mal kann das einige Minuten dauern.")
            .bold()
    ))
    .unwrap();

    let bar = BuildBar::new();
    let result = compile::build(
        &compile::Toolchain::in_tools_dir(tools_dir),
        project,
        &log,
        &bar,
    )
    // Next to this program, where the user finds it.
    .and_then(|executable| compile::copy_to(&executable, project));
    match &result {
        Ok(_) => bar.finish(),
        Err(_) => bar.fail(),
    }
    term.write_line("").unwrap();

    match result {
        Ok(executable) => {
            term.write_line(&format!(
                "{} Die fertige Nummernanzeige liegt hier:\n  {}",
                style("Geschafft!").green().bold(),
                executable.display()
            ))
            .unwrap();
        }
        Err(error) => {
            term.write_line(&format!("{} {error}", style("Fehler:").red().bold()))
                .unwrap();
            term.write_line(&format!(
                "\nDie vollständige Ausgabe steht in {}",
                log.display()
            ))
            .unwrap();
        }
    }

    term.write_line(&format!(
        "\n{}",
        style("Zum Schließen Enter drücken.").dim()
    ))
    .unwrap();
    let _ = term.read_line();
}

fn download_mingw(progress_bar: &ComponentBar) -> Result<(), InstallError> {
    let mut buffer = vec![];
    let mut request = HTTP
        .get(MINGW_DOWNLOAD_FILE)
        .call()
        .map_err(|error| InstallError::download(MINGW_DOWNLOAD_FILE, error))?
        .into_body();
    progress_bar.start_download(request.content_length());
    progress_bar
        .wrap_read(request.as_reader())
        .read_to_end(&mut buffer)
        .map_err(|error| InstallError::download(MINGW_DOWNLOAD_FILE, error))?;
    verify_sha256(MINGW_DOWNLOAD_FILE, &buffer, &MINGW_HASH)?;
    let marker = tool_path(MINGW_MARKER);
    if marker.exists() {
        std::fs::remove_file(&marker).map_err(|e| InstallError::fs("löschen", &marker, e))?;
    }
    unpack::unpack_zip("LLVM-MinGW", buffer, &tool_path(MINGW_DIR), progress_bar)?;
    unpack::write_marker(&marker, MINGW_DOWNLOAD_FILE)
}

fn download_rust(progress_bar: &ComponentBar) -> Result<(), InstallError> {
    let mut bodies = Vec::new();
    for (_, url, _, _) in RUST_PACKAGES {
        let body = HTTP
            .get(url)
            .call()
            .map_err(|error| InstallError::download(url, error))?
            .into_body();
        bodies.push(body);
    }
    let sizes: Option<Vec<u64>> = bodies.iter().map(|body| body.content_length()).collect();
    progress_bar.start_download(sizes.map(|sizes| sizes.iter().sum()));
    let mut archives = Vec::new();
    for ((_, url, hash, _), mut body) in RUST_PACKAGES.into_iter().zip(bodies) {
        let mut buffer = vec![];
        progress_bar
            .wrap_read(body.as_reader())
            .read_to_end(&mut buffer)
            .map_err(|error| InstallError::download(url, error))?;
        verify_sha256(url, &buffer, &hash)?;
        archives.push(buffer);
    }
    let marker = tool_path(RUST_MARKER);
    if marker.exists() {
        std::fs::remove_file(&marker).map_err(|e| InstallError::fs("löschen", &marker, e))?;
    }
    let rustlib = tool_path("lib/rustlib");
    if rustlib.exists() {
        std::fs::remove_dir_all(&rustlib).map_err(|e| InstallError::fs("löschen", &rustlib, e))?;
    }
    progress_bar.start_unpack(archives.iter().map(|archive| archive.len() as u64).sum());
    for ((name, _, _, component), archive) in RUST_PACKAGES.into_iter().zip(&archives) {
        unpack::unpack_rust_component(
            name,
            archive,
            component,
            &tool_path(RUST_DIR),
            progress_bar,
        )?;
    }
    unpack::write_marker(&marker, "1.100.0")
}
