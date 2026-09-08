//! `aoe sandbox`: inspect and reclaim the per-session agent stores that
//! sandboxed sessions mount as their agent's config directory.

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::migrations::progress::format_bytes;
use crate::session::sandbox_store_reclaim as reclaim;

#[derive(Subcommand)]
pub enum SandboxCommands {
    /// Report per-session agent stores whose session no longer exists in any
    /// profile, and how much disk they hold. Reports only unless `--delete`
    /// is given: each store holds a copy of that agent's credentials.
    Reclaim(ReclaimArgs),
}

#[derive(Args)]
pub struct ReclaimArgs {
    /// Remove the reported stores instead of only naming them.
    #[arg(long)]
    pub delete: bool,
}

pub fn run(command: SandboxCommands) -> Result<()> {
    match command {
        SandboxCommands::Reclaim(args) => run_reclaim(args),
    }
}

fn run_reclaim(args: ReclaimArgs) -> Result<()> {
    if !args.delete {
        let plan = reclaim::report()?;
        print_plan(&plan);
        if !plan.orphans.is_empty() {
            println!("\nRun `aoe sandbox reclaim --delete` to remove them.");
        }
        return Ok(());
    }

    let outcome = reclaim::reclaim()?;
    print_plan(&outcome.plan);
    println!();
    if outcome.removed.is_empty() {
        println!("Removed nothing.");
    } else {
        println!(
            "Removed {} store(s), freeing {}.",
            outcome.removed.len(),
            format_bytes(outcome.freed())
        );
    }
    for (path, reason) in &outcome.failures {
        eprintln!("aoe: kept {}: {reason}", path.display());
    }
    Ok(())
}

fn print_plan(plan: &reclaim::Plan) {
    println!(
        "Scanned {} store root(s) against {} session(s) across all profiles.",
        plan.roots.len(),
        plan.owners
    );
    for (path, reason) in &plan.preserved {
        println!("  preserved  {}  ({})", path.display(), reason.label());
    }
    if plan.orphans.is_empty() {
        println!("No orphaned agent stores.");
        return;
    }
    for orphan in &plan.orphans {
        println!(
            "  orphan     {}  ({})",
            orphan.path.display(),
            format_bytes(orphan.bytes)
        );
    }
    println!(
        "{} orphaned store(s), {} total.",
        plan.orphans.len(),
        format_bytes(plan.bytes())
    );
}
