//! `aoe cityhall` subcommands: produce and consume the CityHall config bundle.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueHint};
use reqwest::Method;
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

pub async fn run(command: CityHallCommands) -> Result<()> {
    match command {
        CityHallCommands::Export(args) => run_export(args),
        CityHallCommands::Apply(args) => run_apply(args).await,
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

/// Applying clones repositories, which outlasts the client's default timeout.
const APPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

async fn run_apply(args: ApplyArgs) -> Result<()> {
    let raw = if args.file == "-" {
        std::io::read_to_string(std::io::stdin()).context("reading the bundle from stdin")?
    } else {
        std::fs::read_to_string(&args.file).with_context(|| format!("reading {}", args.file))?
    };

    // Fail on a malformed bundle before starting a Daemon for it.
    CityHallBundle::from_toml(&raw)?;
    let value = super::daemon_client()
        .await?
        .mutate_with(Method::POST, "/api/cityhall/apply", |request| {
            request
                .header(reqwest::header::CONTENT_TYPE, "application/toml")
                .body(raw.clone())
                .timeout(APPLY_TIMEOUT)
        })
        .await?;
    let report: cityhall_bundle::ApplyReport = serde_json::from_value(value)?;

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
    for failure in &report.failures {
        eprintln!("Warning: {failure}");
    }
    if nothing_applied(&report) {
        bail!("no project could be applied");
    }
    Ok(())
}

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
            (report(&[], &[], &["kept"], &["a: clone failed"]), false),
            (report(&["new"], &[], &[], &["a: clone failed"]), false),
            (report(&[], &["reg"], &[], &["a: clone failed"]), false),
            (report(&[], &[], &[], &[]), false),
            (report(&[], &[], &["kept"], &[]), false),
        ];
        for (report, expected) in cases {
            assert_eq!(nothing_applied(&report), expected, "{report:?}");
        }
    }
}
