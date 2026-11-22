//! Shows the progress bars with simulated work: `cargo run --example progress_demo`.
//! Pass `one` to see the case where only one component is missing, and `fail`
//! to see failures (a connection breaking off, a checksum mismatch).

#[allow(dead_code)] // the demo does not use every method
#[path = "../src/error.rs"]
mod error;
#[allow(dead_code)]
#[path = "../src/progress.rs"]
mod progress;

use std::{io, thread, time::Duration};

use error::InstallError;
use progress::{ComponentBar, Progress};

/// What goes wrong in the simulation, if anything.
#[derive(Clone, Copy)]
enum Failure {
    None,
    /// The connection breaks off halfway through the download.
    ConnectionLost,
    /// The download completes, but its checksum is wrong.
    Checksum,
}

fn simulate(
    bar: &ComponentBar,
    download: u64,
    archive: u64,
    speed: u64,
    failure: Failure,
) -> Result<(), InstallError> {
    let step = Duration::from_millis(50);
    let url = "https://example.invalid/tool.tar.gz";

    bar.start_download(Some(download));
    for done in (0..download).step_by(speed as usize) {
        if matches!(failure, Failure::ConnectionLost) && done >= download / 2 {
            let reset = io::Error::from(io::ErrorKind::ConnectionReset);
            return Err(InstallError::download(url, reset));
        }
        thread::sleep(step);
        bar.inc(speed);
    }
    if matches!(failure, Failure::Checksum) {
        error::verify_sha256(url, b"not the pinned file", &[0; 32])?;
    }

    bar.start_unpack(archive);
    for _ in 0..archive / (speed * 3) {
        thread::sleep(step);
        bar.inc(speed * 3);
    }
    Ok(())
}

fn main() {
    let only_one = std::env::args().any(|arg| arg == "one");
    let fail = std::env::args().any(|arg| arg == "fail");
    let progress = Progress::new();
    let mut workers = Vec::new();

    if !only_one {
        let rust = progress.add("Rust");
        let failure = if fail { Failure::ConnectionLost } else { Failure::None };
        workers.push((
            "Rust",
            thread::spawn(move || {
                rust.run(|bar| simulate(bar, 90 << 20, 90 << 20, 350 << 10, failure))
            }),
        ));
    }
    let mingw = progress.add("LLVM MinGW");
    let failure = if fail { Failure::Checksum } else { Failure::None };
    workers.push((
        "LLVM MinGW",
        thread::spawn(move || {
            mingw.run(|bar| simulate(bar, 150 << 20, 150 << 20, 700 << 10, failure))
        }),
    ));

    // Wait for every worker before printing, or the text garbles the other bar.
    let results: Vec<_> = workers
        .into_iter()
        .map(|(name, worker)| (name, worker.join().unwrap()))
        .collect();

    let mut first = true;
    for (name, result) in results {
        if let Err(error) = result {
            if first {
                eprintln!();
                first = false;
            }
            eprintln!("{name}: {error}");
        }
    }
}
