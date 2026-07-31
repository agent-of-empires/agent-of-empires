//! `aoe cityhall` subcommands: produce and consume the CityHall config bundle.
//!
//! `export` runs on an admin's own machine; `apply` runs inside a CityHall
//! workspace (normally driven automatically at `aoe serve` boot, see
//! `crate::cli::serve`). See `crate::session::cityhall_bundle`.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueHint};
use std::path::PathBuf;

use crate::session::cityhall_bundle::{self, CityHallBundle};

#[derive(Subcommand)]
pub enum CityHallCommands {
    /// Write a bundle describing this install's settings and projects
    Export(ExportArgs),

    /// Apply a bundle to this install (merge settings, clone and register
    /// projects, install the git identity)
    Apply(ApplyArgs),
}

#[derive(Args)]
pub struct ExportArgs {
    /// Write to a file instead of stdout
    #[arg(long, short, value_hint = ValueHint::FilePath)]
    out: Option<PathBuf>,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Bundle to apply; `-` reads stdin
    #[arg(value_hint = ValueHint::FilePath)]
    file: String,
}

pub fn run(command: CityHallCommands) -> Result<()> {
    match command {
        CityHallCommands::Export(args) => run_export(args),
        CityHallCommands::Apply(args) => run_apply(args),
    }
}

fn run_export(args: ExportArgs) -> Result<()> {
    let toml = cityhall_bundle::export()?.to_toml()?;
    match args.out {
        Some(path) => {
            std::fs::write(&path, &toml).with_context(|| format!("writing {}", path.display()))?;
            println!("Wrote {}", path.display());
        }
        None => print!("{toml}"),
    }
    Ok(())
}

fn run_apply(args: ApplyArgs) -> Result<()> {
    let raw = if args.file == "-" {
        std::io::read_to_string(std::io::stdin()).context("reading the bundle from stdin")?
    } else {
        std::fs::read_to_string(&args.file).with_context(|| format!("reading {}", args.file))?
    };

    let report = cityhall_bundle::apply(&CityHallBundle::from_toml(&raw)?)?;

    println!("Applied {} settings.", report.settings_applied);
    if !report.cloned.is_empty() {
        println!("Cloned: {}", report.cloned.join(", "));
    }
    if !report.registered.is_empty() {
        println!("Registered: {}", report.registered.join(", "));
    }
    // Project failures are collected rather than fatal, so surface them here
    // instead of letting a partial apply look like a clean one.
    for failure in &report.failures {
        eprintln!("Warning: {failure}");
    }
    // A partial apply stays a success: the other projects landed, and the boot
    // path depends on that. But when nothing landed at all, a script has no way
    // to tell this from a clean run, so fail.
    if !report.failures.is_empty() && report.cloned.is_empty() && report.registered.is_empty() {
        bail!("no project could be applied");
    }
    Ok(())
}
