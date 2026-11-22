//! Building the Nummernanzeige with the tools in `.tools`, shown as one
//! progress line instead of Cargo's own output.

use std::{
    error::Error,
    fmt, fs,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
};

use cargo_metadata::{Message, diagnostic::DiagnosticLevel};

use crate::progress::BuildBar;

pub const TARGET: &str = "x86_64-pc-windows-gnullvm";

/// Variables that would make Cargo build differently than intended, e.g.
/// left over from a Rust installation the user already has.
const IGNORED_ENV: &[&str] = &[
    "RUSTUP_TOOLCHAIN",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "RUSTC_BOOTSTRAP",
];

/// Where Cargo, rustc and the C toolchain are.
pub struct Toolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    /// Put in front of `PATH`: Cargo looks up the linker
    /// (`x86_64-w64-mingw32-clang`) and `windres` there.
    pub path: Vec<PathBuf>,
    /// Cargo's own folder (downloaded crates, the GPUI git checkout).
    pub cargo_home: PathBuf,
}

impl Toolchain {
    /// The layout `main` installs into `.tools`.
    pub fn in_tools_dir(tools: &Path) -> Self {
        Self {
            cargo: tools.join("bin").join("cargo.exe"),
            rustc: tools.join("bin").join("rustc.exe"),
            path: vec![tools.join("llvm-mingw").join("bin"), tools.join("bin")],
            cargo_home: tools.join("cargo-home"),
        }
    }

    fn cargo(&self, project: &Path, args: &[&str]) -> Result<Command, BuildError> {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            self.path
                .iter()
                .cloned()
                .chain(std::env::split_paths(&inherited)),
        )
        .map_err(|error| BuildError::Start(io::Error::other(error)))?;

        let mut command = Command::new(&self.cargo);
        command
            .args(args)
            // Not only `--manifest-path`: Cargo finds `.cargo/config.toml`
            // (linker settings, `crt-static`) from the current folder.
            .current_dir(project)
            .env("PATH", path)
            .env("RUSTC", &self.rustc)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in IGNORED_ENV {
            command.env_remove(name);
        }
        Ok(command)
    }
}

/// Builds `project` in release mode for Windows and returns the path of the
/// finished `.exe`. Cargo's complete output is written to `log`.
pub fn build(
    toolchain: &Toolchain,
    project: &Path,
    log: &Path,
    bar: &BuildBar,
) -> Result<PathBuf, BuildError> {
    if !project.join("Cargo.toml").exists() {
        return Err(BuildError::MissingProject(project.to_owned()));
    }
    let mut transcript = Vec::new();
    let result = build_logged(toolchain, project, bar, &mut transcript);
    // The log matters most when something failed, so it is written either way;
    // not being able to write it is not worth failing over.
    let _ = fs::write(log, transcript.join("\n"));
    result
}

fn build_logged(
    toolchain: &Toolchain,
    project: &Path,
    bar: &BuildBar,
    transcript: &mut Vec<String>,
) -> Result<PathBuf, BuildError> {
    let common = ["build", "--release", "--locked", "--target", TARGET];

    // Step 1: the unit graph. Resolving it makes Cargo download every
    // dependency, and it lists exactly the compile steps the build will report.
    // `--unit-graph` is only available in nightly mode; `RUSTC_BOOTSTRAP`
    // enables that on a stable Cargo. Safe here, because the Cargo version is
    // pinned, so the output format cannot change under us.
    let mut command = toolchain.cargo(
        project,
        &[&common[..], &["--unit-graph", "-Z", "unstable-options"]].concat(),
    )?;
    command.env("RUSTC_BOOTSTRAP", "1");
    let (status, graph, stderr) = run(command, bar, |stdout| {
        let mut json = String::new();
        stdout.read_to_string(&mut json).map(|_| json)
    })?;
    transcript.extend(stderr.iter().cloned());
    if !status.success() {
        return Err(BuildError::Prepare {
            output: tail(&stderr),
        });
    }
    // Without a total, the bar still counts, just without an end.
    if let Some(total) = graph.ok().and_then(|json| count_compile_units(&json)) {
        bar.set_total(total);
    }

    // Step 2: the build itself.
    let command = toolchain.cargo(project, &[&common[..], &["--message-format=json"]].concat())?;
    let (status, outcome, stderr) = run(command, bar, |stdout| {
        let mut outcome = BuildOutcome::default();
        for message in Message::parse_stream(BufReader::new(stdout)) {
            match message? {
                Message::CompilerArtifact(artifact) => {
                    bar.step();
                    if artifact.fresh {
                        // Up to date already: Cargo announces no "Compiling" for it.
                        bar.set_crate(&artifact.target.name);
                    }
                    if let Some(executable) = artifact.executable {
                        outcome.executable = Some(executable.into_std_path_buf());
                    }
                }
                Message::CompilerMessage(message) => {
                    let diagnostic = message.message;
                    let rendered = diagnostic.rendered.unwrap_or(diagnostic.message);
                    if matches!(
                        diagnostic.level,
                        DiagnosticLevel::Error | DiagnosticLevel::Ice
                    ) {
                        outcome.errors.push(rendered.clone());
                    }
                    outcome.diagnostics.push(rendered);
                }
                Message::TextLine(line) => outcome.diagnostics.push(line),
                _ => {}
            }
        }
        Ok(outcome)
    })?;
    transcript.extend(stderr.iter().cloned());

    let outcome = outcome.map_err(BuildError::Start)?;
    transcript.extend(outcome.diagnostics);
    if !status.success() {
        return Err(BuildError::Compile {
            errors: outcome.errors,
            output: tail(&stderr),
        });
    }
    Ok(outcome.executable.unwrap_or_else(|| {
        project
            .join("target")
            .join(TARGET)
            .join("release")
            .join("nummernanzeige.exe")
    }))
}

#[derive(Default)]
struct BuildOutcome {
    executable: Option<PathBuf>,
    /// Compiler errors, as the compiler renders them.
    errors: Vec<String>,
    /// Everything the compiler said, warnings included (for the log).
    diagnostics: Vec<String>,
}

/// Runs Cargo, hands its standard output to `read_stdout`, and collects its
/// standard error, where Cargo writes its status lines ("Compiling gpui …").
/// Those set the name on the bar as they arrive.
fn run<T>(
    mut command: Command,
    bar: &BuildBar,
    read_stdout: impl FnOnce(&mut dyn Read) -> io::Result<T>,
) -> Result<(ExitStatus, io::Result<T>, Vec<String>), BuildError> {
    let mut child: Child = command.spawn().map_err(BuildError::Start)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let status_bar = bar.clone();
    let stderr_reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if let Some(label) = status_label(&line) {
                status_bar.set_crate(&label);
            }
            lines.push(line);
        }
        lines
    });

    // Read standard output to the end before waiting, or Cargo could block
    // on a full pipe.
    let output = read_stdout(&mut stdout);
    drop(stdout);
    let status = child.wait().map_err(BuildError::Start)?;
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok((status, output, stderr))
}

/// What a Cargo status line is about, for the name column:
/// `   Compiling gpui_ce v0.4.0 (…)` gives `gpui_ce`.
fn status_label(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    match (words.next()?, words.next()) {
        ("Compiling" | "Downloaded" | "Checking", Some(name)) => Some(name.to_owned()),
        ("Updating", _) => Some("Paketliste laden".to_owned()),
        ("Downloading", _) => Some("Herunterladen".to_owned()),
        _ => None,
    }
}

/// Counts the units in `cargo build --unit-graph` output that report a
/// `compiler-artifact` when built: all except build-script runs.
fn count_compile_units(json: &str) -> Option<u64> {
    let graph: serde_json::Value = serde_json::from_str(json).ok()?;
    let units = graph.get("units")?.as_array()?;
    let count = units
        .iter()
        .filter(|unit| unit.get("mode").and_then(|mode| mode.as_str()) == Some("build"))
        .count();
    Some(count as u64)
}

/// The last lines of Cargo's status output, for when there is nothing better.
fn tail(lines: &[String]) -> String {
    let start = lines.len().saturating_sub(25);
    lines[start..].join("\n")
}

/// Copies the built `executable` into `folder` (the project's folder, where
/// the user finds it), replacing an older copy, and returns the copy's path.
pub fn copy_to(executable: &Path, folder: &Path) -> Result<PathBuf, BuildError> {
    let name = executable
        .file_name()
        .unwrap_or("nummernanzeige.exe".as_ref());
    let destination = folder.join(name);
    fs::copy(executable, &destination).map_err(|source| BuildError::Copy {
        destination: destination.clone(),
        source,
    })?;
    Ok(destination)
}

#[derive(Debug)]
pub enum BuildError {
    /// There is no `Cargo.toml` next to this program.
    MissingProject(PathBuf),
    /// Cargo could not be started, or its output not be read.
    Start(io::Error),
    /// Cargo failed before compiling, usually while downloading dependencies.
    Prepare { output: String },
    /// The build failed.
    Compile { errors: Vec<String>, output: String },
    /// The finished program could not be copied next to this one.
    Copy {
        destination: PathBuf,
        source: io::Error,
    },
}

/// Windows' "file in use by another process" (`ERROR_SHARING_VIOLATION`).
const ERROR_SHARING_VIOLATION: i32 = 32;

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingProject(folder) => write!(
                f,
                "Im Ordner {} liegt kein Projekt (Cargo.toml fehlt). Dieses Programm muss im \
                 Ordner der Nummernanzeige liegen.",
                folder.display()
            ),
            Self::Start(error) => write!(f, "Cargo konnte nicht gestartet werden.\n  ({error})"),
            Self::Prepare { output } => write!(
                f,
                "Die Abhängigkeiten konnten nicht geladen werden. Besteht eine \
                 Internetverbindung?\n\n{output}"
            ),
            Self::Compile { errors, output } if errors.is_empty() => {
                write!(f, "Das Kompilieren ist fehlgeschlagen.\n\n{output}")
            }
            Self::Compile { errors, .. } => write!(
                f,
                "Das Kompilieren ist fehlgeschlagen. Der Compiler meldet:\n\n{}",
                errors.join("\n")
            ),
            Self::Copy {
                destination,
                source,
            } if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                write!(
                    f,
                    "Die alte Nummernanzeige ({}) läuft noch und kann deshalb nicht ersetzt \
                     werden. Bitte schließen und dieses Programm noch einmal starten.",
                    destination.display()
                )
            }
            Self::Copy {
                destination,
                source,
            } => write!(
                f,
                "Die fertige Nummernanzeige konnte nicht nach {} kopiert werden.\n  ({source})",
                destination.display()
            ),
        }
    }
}

impl Error for BuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Start(error) => Some(error),
            Self::Copy { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        let label = |line: &str| status_label(line);
        assert_eq!(
            label("   Compiling gpui_ce v0.4.0 (https://…)").as_deref(),
            Some("gpui_ce")
        );
        assert_eq!(
            label("  Downloaded serde v1.0.229").as_deref(),
            Some("serde")
        );
        assert_eq!(
            label("    Updating crates.io index").as_deref(),
            Some("Paketliste laden")
        );
        assert_eq!(label("warning: unused variable"), None);
        assert_eq!(label("    Finished `release` profile"), None);
    }
}
