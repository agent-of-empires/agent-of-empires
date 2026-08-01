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
    if !report.preserved.is_empty() {
        println!("Already in place: {}", report.preserved.join(", "));
    }
    // Project failures are collected rather than fatal, so surface them here
    // instead of letting a partial apply look like a clean one.
    for failure in &report.failures {
        eprintln!("Warning: {failure}");
    }
    if nothing_applied(&report) {
        bail!("no project could be applied");
    }
    Ok(())
}

/// Whether an apply that reported failures managed to land nothing at all.
///
/// A partial apply stays a success: the other projects are in place, and the
/// boot path depends on that. Only a run where every project failed is worth a
/// non-zero exit, because a script cannot otherwise tell it from a clean one. A
/// project that was already cloned and already registered counts as landed;
/// without that, re-applying an unchanged bundle alongside one bad remote would
/// look like a total failure.
fn nothing_applied(report: &cityhall_bundle::ApplyReport) -> bool {
    !report.failures.is_empty()
        && report.cloned.is_empty()
        && report.registered.is_empty()
        && report.preserved.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cityhall_bundle::ApplyReport;

    fn report(
        cloned: &[&str],
        registered: &[&str],
        preserved: &[&str],
        failures: &[&str],
    ) -> ApplyReport {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        ApplyReport {
            settings_applied: 0,
            cloned: own(cloned),
            registered: own(registered),
            preserved: own(preserved),
            failures: own(failures),
        }
    }

    #[test]
    fn only_a_totally_failed_apply_is_an_error() {
        let cases = [
            (report(&[], &[], &[], &["a: clone failed"]), true),
            // The regression this guards: one repo already in place next to one
            // bad remote is a partial apply, not a total failure.
            (report(&[], &[], &["kept"], &["a: clone failed"]), false),
            (report(&["new"], &[], &[], &["a: clone failed"]), false),
            (report(&[], &["reg"], &[], &["a: clone failed"]), false),
            // No failures at all is never an error, including a pure no-op.
            (report(&[], &[], &[], &[]), false),
            (report(&[], &[], &["kept"], &[]), false),
        ];
        for (report, expected) in cases {
            assert_eq!(nothing_applied(&report), expected, "{report:?}");
        }
    }
}
