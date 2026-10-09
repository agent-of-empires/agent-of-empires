use crate::session::AnchoredDir;
use anyhow::{Context, Result};
use std::path::Path;

pub(crate) fn read_at(app: &AnchoredDir) -> Result<u32> {
    let marker = Path::new(super::VERSION_FILE);
    match app.regular_lookup(marker)? {
        None => return Ok(0),
        Some(false) => anyhow::bail!("data schema marker is not a regular file"),
        Some(true) => {}
    }
    let bytes = app
        .read_regular(marker, usize::MAX)?
        .context("data schema marker disappeared while reading")?;
    std::str::from_utf8(&bytes)
        .context("data schema marker is not UTF-8")?
        .trim()
        .parse()
        .context("invalid data schema version")
}
