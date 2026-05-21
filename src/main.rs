mod state;

#[cfg(target_os = "macos")]
mod macos;

use std::error::Error;

fn main() {
    if let Err(error) = run() {
        eprintln!("hypermakey: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args();
    let _program = args.next();

    match args.next().as_deref() {
        Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("hypermakey {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(argument) => Err(format!("unknown argument: {argument}").into()),
        None => run_daemon(),
    }
}

fn print_help() {
    println!(
        "hypermakey {}\n\nUsage:\n  hypermakey\n  hypermakey --version\n  hypermakey --help\n\nRuns a minimal macOS Caps Lock to Hyper Key daemon.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(target_os = "macos")]
fn run_daemon() -> Result<(), Box<dyn Error>> {
    macos::run().map_err(Into::into)
}

#[cfg(not(target_os = "macos"))]
fn run_daemon() -> Result<(), Box<dyn Error>> {
    Err("hypermakey only supports macOS".into())
}
