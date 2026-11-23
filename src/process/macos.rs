//! macOS-specific process utilities.

use anyhow::Context as _;
use std::collections::HashMap;
use std::process::Command;

pub(super) use super::unix::{
    close_frozen_payload, configure_process_group, exec_command, kill_process_group,
    restore_environment_value, terminate_process_group, try_wait_status_hook,
};

/// A fixed-size disk image. The caller owns the recovery marker and must settle
/// every Git writer before calling `close`. Deliberately no Drop cleanup: an
/// interrupted attach or detach must leave its image available for recovery.
pub(super) struct StagingVolume {
    root: std::path::PathBuf,
    image: std::path::PathBuf,
    image_file: Option<std::fs::File>,
    image_identity: (u64, u64),
    image_length: u64,
    mount_identity: (u64, u64),
    underlying_identity: (u64, u64),
    device: String,
    limit: u64,
    closed: bool,
}

// UDRW adds a trailer and may round its allocation to host filesystem blocks.
// Leave room for both instead of advertising the whole budget as usable space.
const STAGING_IMAGE_OVERHEAD: u64 = 8 * 1024 * 1024;
const STAGING_TOOL_OUTPUT: usize = 1024 * 1024;
// This bounds read-only catalogue convergence, not attachment or quota enforcement.
const STAGING_INVENTORY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(test)]
std::thread_local! {
    static FAIL_STAGING_ATTACH_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, thiserror::Error)]
#[error("spawning bounded macOS staging tool: {0}")]
struct StagingToolNotStarted(#[source] std::io::Error);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct StagingInventoryMissing(String);

const STAGING_SANDBOX: &str = r#"
(version 1)
(deny default)
; These commands can mutate files through read-only descriptors, bypassing
; file-write*. Deny them explicitly even in a deny-default profile.
(deny system-fcntl (fcntl-command 80 110))
(allow process-exec process-fork)
(allow signal (target self))
(allow file-read*)
(allow sysctl-read)
(allow file-write* (subpath (param "AOE_STAGING_ROOT")))
; Git opens this character device O_RDWR. Reads and metadata are covered above;
; only data writes are needed, not host metadata writes or arbitrary device I/O.
(allow file-write-data (literal "/dev/null"))
(allow network-outbound (remote tcp))
(allow network-outbound (remote udp "*:53"))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.SystemConfiguration.configd")
    (global-name "com.apple.mDNSResponder")
    (global-name "com.apple.trustd")
    (global-name "com.apple.trustd.agent"))
(allow ipc-posix-shm-read-data ipc-posix-shm-read-metadata
    (ipc-posix-name "apple.shm.notification_center"))
"#;

pub(super) fn prove_staging_host_mount_absent(
    parent: std::os::fd::BorrowedFd<'_>,
    child: std::os::fd::BorrowedFd<'_>,
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd as _;
    let mount = |fd: std::os::fd::BorrowedFd<'_>| -> anyhow::Result<_> {
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: fd is borrowed and stat points to writable storage.
        if unsafe { libc::fstatfs(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("inspecting staging mount");
        }
        // SAFETY: successful fstatfs initialized the result.
        let stat = unsafe { stat.assume_init() };
        Ok((stat.f_mntonname, stat.f_mntfromname))
    };
    anyhow::ensure!(
        mount(parent)? == mount(child)?,
        "plugin staging path remains mounted"
    );
    Ok(())
}

impl StagingVolume {
    #[cfg(test)]
    pub(super) fn create(private_mount_path: &std::path::Path, limit: u64) -> anyhow::Result<Self> {
        Self::create_recorded(private_mount_path, limit, |_| Ok(tempfile::tempfile()?))
    }

    pub(super) fn create_recorded(
        private_mount_path: &std::path::Path,
        limit: u64,
        mut record: impl FnMut(super::StagingCustody) -> anyhow::Result<std::fs::File>,
    ) -> anyhow::Result<Self> {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        #[cfg(test)]
        let fail_attach_spawn = FAIL_STAGING_ATTACH_SPAWN.with(|fail| fail.replace(false));
        let metadata = std::fs::symlink_metadata(private_mount_path)?;
        anyhow::ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "plugin staging mountpoint must be a private owned directory"
        );
        anyhow::ensure!(
            std::fs::read_dir(private_mount_path)?.next().is_none(),
            "plugin staging mountpoint must be empty"
        );
        let root = private_mount_path.canonicalize()?;
        root.to_str()
            .context("plugin staging mountpoint must be valid UTF-8 for sandbox-exec")?;
        let parent = root
            .parent()
            .context("plugin staging mountpoint has no parent")?;
        let parent_metadata = std::fs::metadata(parent)?;
        anyhow::ensure!(
            parent_metadata.uid() == unsafe { libc::geteuid() }
                && parent_metadata.mode() & 0o077 == 0
                && parent_metadata.dev() == metadata.dev(),
            "plugin staging requires a private parent on the same filesystem"
        );
        anyhow::ensure!(
            staging_mount_strings(&staging_statfs(&root)?)?.0 != root,
            "plugin staging mountpoint is already mounted"
        );
        let capacity = limit
            .checked_sub(STAGING_IMAGE_OVERHEAD)
            .context("plugin staging budget is too small for a fixed disk image")?
            / 4096
            * 4096;
        anyhow::ensure!(
            capacity >= 8 * 1024 * 1024,
            "plugin staging disk image is too small"
        );
        let image = root.with_extension("dmg");
        anyhow::ensure!(
            std::fs::symlink_metadata(&image)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "plugin staging image already exists or cannot be inspected"
        );
        // Validate availability before allocating the image. Failure is not a
        // reason to fall back to an unbounded directory.
        staging_fd_ceiling()?;
        anyhow::ensure!(
            std::fs::metadata("/usr/bin/sandbox-exec")?.is_file(),
            "macOS plugin staging requires sandbox-exec"
        );
        anyhow::ensure!(
            std::fs::metadata("/usr/bin/hdiutil")?.is_file(),
            "macOS plugin staging requires hdiutil"
        );
        let mut create = Command::new("/usr/bin/hdiutil");
        create
            .args([
                "create",
                "-type",
                "UDIF",
                "-layout",
                "NONE",
                "-fs",
                "HFSX",
                "-volname",
                "aoe-plugin-staging",
                "-mode",
                "0700",
                "-uid",
            ])
            .arg(unsafe { libc::geteuid() }.to_string())
            .arg("-gid")
            .arg(unsafe { libc::getegid() }.to_string())
            .arg("-sectors")
            .arg((capacity / 512).to_string())
            .arg(&image);
        let mut created = false;
        let mut attachment_may_exist = false;
        // A disk image can outlive its creator and its daemon. Until a future
        // native inventory proves ownership and detach, retain crash evidence.
        record(super::StagingCustody::Uncertain)?;
        let result: anyhow::Result<Self> = (|| {
            staging_tool(&mut create)?;
            created = true;
            let image_file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&image)?;
            let image_metadata = image_file.metadata()?;
            anyhow::ensure!(
                image_metadata.is_file()
                    && image_metadata.nlink() == 1
                    && image_metadata.uid() == unsafe { libc::geteuid() }
                    && image_metadata.len() >= capacity
                    && image_metadata.len() <= limit
                    && image_metadata
                        .blocks()
                        .checked_mul(512)
                        .is_some_and(|bytes| bytes <= limit),
                "plugin staging image identity or size is invalid"
            );
            // UDRW has a fixed logical backing length, unlike UDSP/UDSB. Host
            // filesystem holes do not increase that bound and need not be filled.
            let mut attach = Command::new("/usr/bin/hdiutil");
            attach
                .args([
                    "attach",
                    "-plist",
                    "-nobrowse",
                    "-noautoopen",
                    "-noautofsck",
                    "-owners",
                    "on",
                    "-readwrite",
                    "-mountpoint",
                ])
                .arg(&root)
                .arg(&image);
            #[cfg(test)]
            if fail_attach_spawn {
                use std::os::unix::process::CommandExt as _;
                // SAFETY: only constructs an OS error; Rust's spawn-error pipe
                // returns EAGAIN before hdiutil executes or acquires the image.
                unsafe {
                    attach.pre_exec(|| Err(std::io::Error::from_raw_os_error(libc::EAGAIN)));
                }
            }
            attachment_may_exist = true;
            let output = staging_tool(&mut attach).inspect_err(|error| {
                if error.downcast_ref::<StagingToolNotStarted>().is_some() {
                    attachment_may_exist = false;
                }
            })?;
            let attached = staging_plist(&output)?;
            let device = staging_attached_device(&attached, &root)?;
            let mount_metadata = std::fs::symlink_metadata(&root)?;
            let mut volume = Self {
                root: root.clone(),
                image: image.clone(),
                image_identity: metadata_identity(&image_metadata),
                image_length: image_metadata.len(),
                image_file: Some(image_file),
                mount_identity: metadata_identity(&mount_metadata),
                underlying_identity: metadata_identity(&metadata),
                device,
                limit,
                closed: false,
            };
            // The attach reply and statfs do not prove that a separate catalogue
            // snapshot already includes this image. Absence is never ownership
            // proof: wait only while both anchored identities remain valid, and
            // require one exact inventory record before admitting any writer.
            volume.wait_for_inventory()?;
            // Prove that the installed policy compiles and can launch a child now,
            // not only when the first untrusted checkout eventually happens.
            let mut probe = volume.command("/usr/bin/true");
            if let Err(error) = staging_tool(&mut probe) {
                if error
                    .downcast_ref::<super::StagingAcquisitionUncertain>()
                    .is_some()
                {
                    return Err(error);
                }
                return match volume.close() {
                    Ok(()) => {
                        // close proved detach, underlying mountpoint restoration
                        // and image removal; no asynchronous attachment remains.
                        attachment_may_exist = false;
                        Err(error)
                    }
                    Err(cleanup) => Err(error.context(super::StagingAcquisitionUncertain(
                        format!("cannot clean up rejected macOS staging sandbox: {cleanup:#}"),
                    ))),
                };
            }
            Ok(volume)
        })();
        match result {
            Ok(volume) => Ok(volume),
            Err(error) => {
                if error
                    .downcast_ref::<super::StagingAcquisitionUncertain>()
                    .is_some()
                {
                    return Err(error);
                }
                let unchanged_mountpoint = std::fs::symlink_metadata(&root).is_ok_and(|current| {
                    metadata_identity(&current) == metadata_identity(&metadata)
                });
                if !attachment_may_exist
                    && unchanged_mountpoint
                    && std::fs::symlink_metadata(&image)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    return Err(error);
                }
                // Before attach, a completed create can be rolled back only
                // after inventory proves no device still owns its image. Once
                // attach starts, absence cannot prove safe custody: retain the
                // image and recovery marker rather than detach an unproven device.
                if created
                    && !attachment_may_exist
                    && unchanged_mountpoint
                    && staging_remove_unattached_image(&image).is_ok()
                {
                    return Err(error);
                }
                Err(error.context(super::StagingAcquisitionUncertain(format!(
                    "macOS staging acquisition left unproven image or mount state at {}",
                    image.display()
                ))))
            }
        }
    }

    pub(super) fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub(super) fn limit(&self) -> u64 {
        self.limit
    }

    pub(super) fn git_command(&self) -> Command {
        self.command("git")
    }

    fn command(&self, program: &str) -> Command {
        use std::os::unix::process::CommandExt as _;
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-p")
            .arg(STAGING_SANDBOX)
            .arg("-D")
            .arg(format!("AOE_STAGING_ROOT={}", self.root.display()))
            .arg(program)
            .current_dir(&self.root)
            .env("TMPDIR", &self.root)
            .env("TMP", &self.root)
            .env("TEMP", &self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Determine the exhaustive range before fork. In the child only fcntl
        // and errno access run; notably, do not close Rust's exec-error pipe.
        let ceiling = staging_fd_ceiling().map_err(|e| e.raw_os_error().unwrap_or(libc::EIO));
        let closed = self.closed;
        unsafe {
            command.pre_exec(move || {
                if closed {
                    return Err(std::io::Error::from_raw_os_error(libc::EBADF));
                }
                let ceiling = ceiling.map_err(std::io::Error::from_raw_os_error)?;
                staging_cloexec_descriptors(ceiling)
            });
        }
        command
    }

    fn verify_mounted(&self) -> anyhow::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        let image = self
            .image_file
            .as_ref()
            .context("plugin staging image is closed")?
            .metadata()?;
        anyhow::ensure!(
            image.is_file()
                && image.nlink() == 1
                && image.uid() == unsafe { libc::geteuid() }
                && metadata_identity(&std::fs::symlink_metadata(&self.image)?)
                    == self.image_identity
                && metadata_identity(&image) == self.image_identity
                && image.len() == self.image_length
                && image
                    .blocks()
                    .checked_mul(512)
                    .is_some_and(|bytes| bytes <= self.limit),
            "plugin staging image changed identity or exceeded its physical budget"
        );
        let metadata = std::fs::symlink_metadata(&self.root)?;
        let stat = staging_statfs(&self.root)?;
        let (mounted_on, mounted_from) = staging_mount_strings(&stat)?;
        anyhow::ensure!(
            metadata_identity(&metadata) == self.mount_identity
                && self.mount_identity.0 != self.underlying_identity.0
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && mounted_on == self.root
                && mounted_from == self.device
                && stat.f_flags & (libc::MNT_RDONLY as u32 | libc::MNT_IGNORE_OWNERSHIP as u32)
                    == 0
                && stat
                    .f_blocks
                    .checked_mul(u64::from(stat.f_bsize))
                    .is_some_and(|bytes| bytes > 0 && bytes <= image.len()),
            "plugin staging mount identity, ownership or capacity is invalid"
        );
        Ok(())
    }

    fn verify_inventory(&self, attached: bool) -> anyhow::Result<()> {
        self.verify_inventory_before(
            attached,
            std::time::Instant::now() + std::time::Duration::from_secs(90),
        )
    }

    fn verify_inventory_before(
        &self,
        attached: bool,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        let mut command = Command::new("/usr/bin/hdiutil");
        command.args(["info", "-plist"]);
        let inventory = staging_plist(&staging_tool_before(&mut command, deadline)?)?;
        staging_verify_inventory(
            &inventory,
            &self.image,
            self.image_identity,
            &self.root,
            self.mount_identity,
            &self.device,
            attached,
        )?;
        Ok(())
    }

    fn wait_for_inventory(&self) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let deadline = started + STAGING_INVENTORY_WAIT;
        staging_wait_for_inventory(
            || {
                self.verify_mounted()?;
                let inventory = self.verify_inventory_before(true, deadline);
                // Check even an absent snapshot: loss of the original image,
                // mount, statfs device or ownership ends the wait immediately.
                self.verify_mounted()?;
                inventory
            },
            || started.elapsed(),
            std::thread::sleep,
            STAGING_INVENTORY_WAIT,
        )
    }

    /// The caller must prove all writers stopped before requesting detach.
    pub(super) fn close(&mut self) -> anyhow::Result<()> {
        use std::os::unix::fs::MetadataExt as _;

        if self.closed {
            return Ok(());
        }
        self.wait_for_inventory()?;
        let mut detach = Command::new("/usr/bin/hdiutil");
        // Never force detach: a busy mount disproves writer settlement.
        detach.arg("detach").arg(&self.device);
        staging_tool(&mut detach)?;
        self.verify_inventory(false)?;
        let image_metadata = self
            .image_file
            .as_ref()
            .context("plugin staging image is closed")?
            .metadata()?;
        anyhow::ensure!(
            metadata_identity(&std::fs::symlink_metadata(&self.root)?) == self.underlying_identity
                && staging_mount_strings(&staging_statfs(&self.root)?)?.0 != self.root
                && metadata_identity(&std::fs::symlink_metadata(&self.image)?)
                    == self.image_identity
                && image_metadata.len() == self.image_length
                && image_metadata
                    .blocks()
                    .checked_mul(512)
                    .is_some_and(|bytes| bytes <= self.limit),
            "plugin staging detach could not be proven; image retained"
        );
        std::fs::remove_file(&self.image)?;
        // Release the unlinked image before admitting the next staging volume.
        drop(self.image_file.take());
        self.closed = true;
        Ok(())
    }
}

// Only a complete, noncontradictory snapshot lacking the anchored image may
// converge. Parse errors, unknown image aliases, duplicate records, changed
// ownership and device/mount mismatches are final errors, never retry signals.
fn staging_wait_for_inventory(
    mut observe: impl FnMut() -> anyhow::Result<()>,
    mut elapsed: impl FnMut() -> std::time::Duration,
    mut pause: impl FnMut(std::time::Duration),
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let mut observations = 0;
    let mut first_inventory = None;
    let mut last_inventory = None;
    loop {
        observations += 1;
        match observe() {
            Ok(()) => return Ok(()),
            Err(error) => match error.downcast::<StagingInventoryMissing>() {
                Ok(missing) => {
                    if first_inventory.is_none() {
                        first_inventory = Some(missing);
                    } else {
                        last_inventory = Some(missing);
                    }
                }
                Err(error) => {
                    let duration = elapsed();
                    return match first_inventory.as_ref() {
                        Some(first) => Err(error.context(format!(
                            "macOS staging inventory readiness failed: elapsed={:?}, observations={observations}, first_inventory={first}, last_inventory={}",
                            duration,
                            last_inventory.as_ref().unwrap_or(first),
                        ))),
                        None if duration >= timeout => Err(error.context(format!(
                            "macOS staging inventory readiness failed: elapsed={duration:?}, observations={observations}, first_inventory=<unavailable>, last_inventory=<unavailable>",
                        ))),
                        None => Err(error),
                    };
                }
            },
        }
        let remaining = timeout.saturating_sub(elapsed());
        if !remaining.is_zero() {
            pause(remaining.min(std::time::Duration::from_millis(25)));
        }
        let duration = elapsed();
        if duration >= timeout {
            let first = first_inventory
                .as_ref()
                .expect("observed missing inventory");
            anyhow::bail!(
                "macOS staging inventory did not publish the anchored image before deadline: elapsed={duration:?}, observations={observations}, first_inventory={first}, last_inventory={}",
                last_inventory.as_ref().unwrap_or(first),
            );
        }
    }
}

fn staging_remove_unattached_image(image: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(image)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "cannot safely remove rejected staging image"
    );
    let mut command = Command::new("/usr/bin/hdiutil");
    command.args(["info", "-plist"]);
    let inventory = staging_plist(&staging_tool(&mut command)?)?;
    staging_verify_unattached_inventory(&inventory, metadata_identity(&metadata))?;
    anyhow::ensure!(
        metadata_identity(&std::fs::symlink_metadata(image)?) == metadata_identity(&metadata),
        "rejected staging image changed identity"
    );
    std::fs::remove_file(image)?;
    Ok(())
}

fn staging_verify_unattached_inventory(
    inventory: &core_foundation::dictionary::CFDictionary,
    image_identity: (u64, u64),
) -> anyhow::Result<()> {
    for entry in staging_plist_array(inventory, "images")?.iter() {
        let record = staging_plist_dictionary(*entry)?;
        let path = staging_plist_string(&record, "image-path")?;
        anyhow::ensure!(
            staging_image_identity(std::path::Path::new(&path))? != Some(image_identity),
            "rejected staging image remains attached: {path}"
        );
    }
    Ok(())
}

// Resolve directory aliases through the filesystem, but never follow a replaced
// final image symlink. Compare against the identity held by the open image file.
fn staging_image_identity(path: &std::path::Path) -> anyhow::Result<Option<(u64, u64)>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => {
            let metadata = file.metadata()?;
            Ok(metadata.is_file().then(|| metadata_identity(&metadata)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("opening inventory image {}", path.display()))
        }
    }
}

fn staging_verify_inventory(
    inventory: &core_foundation::dictionary::CFDictionary,
    image: &std::path::Path,
    image_identity: (u64, u64),
    root: &std::path::Path,
    mount_identity: (u64, u64),
    device: &str,
    attached: bool,
) -> anyhow::Result<()> {
    let images = staging_plist_array(inventory, "images")?;
    let mut found = 0;
    let mut paths = Vec::with_capacity(images.len() as usize);
    for entry in images.iter() {
        let record = staging_plist_dictionary(*entry)?;
        let path = staging_plist_string(&record, "image-path")?;
        let identity = staging_image_identity(std::path::Path::new(&path))?;
        anyhow::ensure!(
            identity.is_some() || std::path::Path::new(&path) != image,
            "cannot resolve staging image identity: {path}"
        );
        // Disk numbers can be reused after detach; only file identity proves ownership.
        let matches = identity == Some(image_identity);
        if matches {
            found += 1;
            anyhow::ensure!(
                attached
                    && staging_attached_device(&record, root)? == device
                    && metadata_identity(&std::fs::symlink_metadata(root)?) == mount_identity,
                "plugin staging image remains attached or changed device/mount: {path}"
            );
        }
        if attached && !matches {
            // Unrelated catalogue entries may outlive detach and device reuse.
            // Only a claim on the anchored mount contradicts this attachment.
            if let Some(entities) = staging_plist_optional_array(&record, "system-entities")? {
                for entity in entities.iter() {
                    let entity = staging_plist_dictionary(*entity)?;
                    let claimed_mount = staging_plist_optional_string(&entity, "mount-point")?;
                    let claims_mount = match claimed_mount {
                        Some(path) => {
                            std::path::Path::new(&path) == root
                                || std::fs::metadata(&path).is_ok_and(|metadata| {
                                    metadata_identity(&metadata) == mount_identity
                                })
                        }
                        None => false,
                    };
                    anyhow::ensure!(
                        !claims_mount,
                        "another inventory image claims the staging mount: {path}"
                    );
                }
            }
        }
        paths.push((path, identity));
    }
    let diagnostic = || {
        format!(
            "plugin staging image attachment is ambiguous: found={found}, expected={}, image={} identity={image_identity:?}, inventory={paths:?}",
            usize::from(attached),
            image.display()
        )
    };
    if attached && found == 0 {
        return Err(StagingInventoryMissing(diagnostic()).into());
    }
    anyhow::ensure!(found == usize::from(attached), "{}", diagnostic());
    Ok(())
}

fn staging_statfs(path: &std::path::Path) -> anyhow::Result<libc::statfs> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    anyhow::ensure!(
        unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "cannot inspect staging mount: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful statfs initialized the entire result.
    Ok(unsafe { stat.assume_init() })
}

fn staging_mount_strings(stat: &libc::statfs) -> anyhow::Result<(std::path::PathBuf, String)> {
    use std::os::unix::ffi::OsStrExt as _;
    fn bytes(value: &[libc::c_char]) -> anyhow::Result<&[u8]> {
        let end = value
            .iter()
            .position(|&b| b == 0)
            .context("unterminated mount identity")?;
        // SAFETY: c_char and u8 have identical layouts; the slice remains borrowed.
        Ok(unsafe { std::slice::from_raw_parts(value.as_ptr().cast(), end) })
    }
    Ok((
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes(&stat.f_mntonname)?)),
        std::str::from_utf8(bytes(&stat.f_mntfromname)?)?.to_owned(),
    ))
}

fn staging_plist(bytes: &[u8]) -> anyhow::Result<core_foundation::dictionary::CFDictionary> {
    use core_foundation::{
        data::CFData,
        propertylist::{self, CFPropertyList},
    };
    let (value, _) = propertylist::create_with_data(CFData::from_buffer(bytes), 0)
        .map_err(|error| anyhow::anyhow!("invalid disk image property list: {error}"))?;
    // SAFETY: create_with_data returned an owned Core Foundation property list.
    let value = unsafe { CFPropertyList::wrap_under_create_rule(value) }.into_CFType();
    value
        .downcast()
        .context("disk image property list is not a dictionary")
}

fn staging_plist_dictionary(
    value: *const std::ffi::c_void,
) -> anyhow::Result<core_foundation::dictionary::CFDictionary> {
    use core_foundation::base::{CFType, TCFType as _};
    anyhow::ensure!(!value.is_null(), "null disk image dictionary");
    // SAFETY: values originate from a live CF property-list array.
    unsafe { CFType::wrap_under_get_rule(value) }
        .downcast()
        .context("disk image entity is not a dictionary")
}

fn staging_plist_value(
    dictionary: &core_foundation::dictionary::CFDictionary,
    key: &str,
) -> anyhow::Result<core_foundation::base::CFType> {
    use core_foundation::{
        base::{CFType, TCFType as _},
        string::CFString,
    };
    let field = key;
    let key = CFString::new(key);
    let value = dictionary
        .find(key.as_concrete_TypeRef().cast::<std::ffi::c_void>())
        .with_context(|| format!("missing disk image property-list field {field:?}"))?;
    // SAFETY: find returns a live retained dictionary's CF object.
    Ok(unsafe { CFType::wrap_under_get_rule(*value) })
}

fn staging_plist_array(
    dictionary: &core_foundation::dictionary::CFDictionary,
    key: &str,
) -> anyhow::Result<core_foundation::array::CFArray> {
    staging_plist_value(dictionary, key)?
        .downcast()
        .context("disk image field is not an array")
}

fn staging_plist_string(
    dictionary: &core_foundation::dictionary::CFDictionary,
    key: &str,
) -> anyhow::Result<String> {
    Ok(staging_plist_value(dictionary, key)?
        .downcast::<core_foundation::string::CFString>()
        .context("disk image field is not a string")?
        .to_string())
}

fn staging_plist_optional_array(
    dictionary: &core_foundation::dictionary::CFDictionary,
    key: &str,
) -> anyhow::Result<Option<core_foundation::array::CFArray>> {
    use core_foundation::{
        base::{CFType, TCFType as _},
        string::CFString,
    };
    let key = CFString::new(key);
    dictionary
        .find(key.as_concrete_TypeRef().cast::<std::ffi::c_void>())
        .map(|value| {
            // SAFETY: find returns a live retained dictionary's CF object.
            unsafe { CFType::wrap_under_get_rule(*value) }
                .downcast()
                .context("disk image field is not an array")
        })
        .transpose()
}

fn staging_plist_optional_string(
    dictionary: &core_foundation::dictionary::CFDictionary,
    key: &str,
) -> anyhow::Result<Option<String>> {
    use core_foundation::{
        base::{CFType, TCFType as _},
        string::CFString,
    };
    let key = CFString::new(key);
    dictionary
        .find(key.as_concrete_TypeRef().cast::<std::ffi::c_void>())
        .map(|value| {
            // SAFETY: find returns a live retained dictionary's CF object.
            unsafe { CFType::wrap_under_get_rule(*value) }
                .downcast::<CFString>()
                .context("disk image field is not a string")
                .map(|value| value.to_string())
        })
        .transpose()
}

fn staging_attached_device(
    dictionary: &core_foundation::dictionary::CFDictionary,
    root: &std::path::Path,
) -> anyhow::Result<String> {
    let entities = staging_plist_array(dictionary, "system-entities")?;
    anyhow::ensure!(
        entities.len() == 1,
        "fixed staging image must expose exactly one device"
    );
    let entity = staging_plist_dictionary(*entities.get(0).context("missing image device")?)?;
    let mounted = std::fs::metadata(staging_plist_string(&entity, "mount-point")?)?;
    anyhow::ensure!(
        metadata_identity(&mounted) == metadata_identity(&std::fs::symlink_metadata(root)?),
        "disk image attached at an unexpected mountpoint"
    );
    let device = staging_plist_string(&entity, "dev-entry")?;
    anyhow::ensure!(
        device
            .strip_prefix("/dev/disk")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())),
        "disk image returned an unexpected device identity"
    );
    Ok(device)
}

fn staging_fd_ceiling() -> std::io::Result<i32> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes writable limit storage.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: getrlimit succeeded.
    let limit = unsafe { limit.assume_init() };
    let mut ceiling = 3u64;
    for name in [libc::KERN_MAXFILES, libc::KERN_MAXFILESPERPROC] {
        let mut mib = [libc::CTL_KERN, name];
        let mut value: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&value);
        // SAFETY: the MIB and output buffers have their specified lengths.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut value as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if size != std::mem::size_of_val(&value) || value <= 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }
        ceiling = ceiling.max(value as u64);
    }
    for bound in [limit.rlim_cur, limit.rlim_max] {
        if bound != libc::RLIM_INFINITY {
            ceiling = ceiling.max(bound);
        }
    }
    // Already-open descriptors can survive lowering both resource limits.
    for entry in std::fs::read_dir("/dev/fd")? {
        let name = entry?.file_name();
        let fd = name
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        ceiling = ceiling.max(
            fd.checked_add(1)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?,
        );
    }
    i32::try_from(ceiling).map_err(|_| std::io::Error::from_raw_os_error(libc::EOVERFLOW))
}

fn staging_cloexec_descriptors(ceiling: i32) -> std::io::Result<()> {
    for fd in 3..ceiling {
        // SAFETY: fcntl accepts arbitrary descriptor numbers and is
        // async-signal-safe. Preserve all existing flags and open descriptors.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(error);
        }
        if flags & libc::FD_CLOEXEC == 0
            && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// System disk-image tools are trusted, but their completion still has a
/// deadline. Any failure leaves the backing image and caller's recovery marker.
fn staging_tool(command: &mut Command) -> anyhow::Result<Vec<u8>> {
    staging_tool_before(
        command,
        std::time::Instant::now() + std::time::Duration::from_secs(90),
    )
}

fn staging_tool_before(
    command: &mut Command,
    deadline: std::time::Instant,
) -> anyhow::Result<Vec<u8>> {
    use std::os::fd::AsRawFd as _;
    use std::time::{Duration, Instant};
    anyhow::ensure!(
        Instant::now() < deadline,
        "macOS staging tool deadline expired before spawn"
    );
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(StagingToolNotStarted)?;
    let mut stdout = child.stdout.take().context("staging tool stdout missing")?;
    let mut stderr = child.stderr.take().context("staging tool stderr missing")?;
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both descriptors belong to the live child's pipes.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            anyhow::ensure!(
                flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
                "cannot make staging diagnostics nonblocking"
            );
        }
        fn drain(pipe: &mut impl std::io::Read, bytes: &mut Vec<u8>) -> anyhow::Result<bool> {
            let mut buffer = [0u8; 8192];
            // Bound each pass too: continuously writing output must not defer
            // checking the tool deadline.
            for _ in 0..128 {
                match pipe.read(&mut buffer) {
                    Ok(0) => return Ok(true),
                    Ok(count) => {
                        anyhow::ensure!(
                            bytes.len() + count <= STAGING_TOOL_OUTPUT,
                            "macOS staging tool exceeded diagnostic limit"
                        );
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(false)
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut status = None;
        loop {
            anyhow::ensure!(
                Instant::now() < deadline,
                "macOS staging tool deadline expired"
            );
            let out_done = drain(&mut stdout, &mut out)?;
            let err_done = drain(&mut stderr, &mut err)?;
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status {
                if out_done && err_done {
                    anyhow::ensure!(
                        status.success(),
                        "macOS staging tool failed ({status}): {}",
                        String::from_utf8_lossy(&err)
                    );
                    return Ok(out);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if let Err(error) = result {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err(error);
        }
        let _ = child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() >= deadline => break,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        // Killing hdiutil does not prove an asynchronous DiskImages request
        // stopped. Preserve evidence even if no attachment is visible yet.
        return Err(error.context(super::StagingAcquisitionUncertain(
            "macOS staging tool was interrupted before its completion was proven".to_owned(),
        )));
    }
    result
}

#[cfg(test)]
mod staging_volume_tests {
    #[test]
    fn rejected_acquisition_without_artifacts_is_not_uncertain() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mount = parent.path().join("mount");
        std::fs::create_dir(&mount).unwrap();
        std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = match StagingVolume::create(&mount, STAGING_IMAGE_OVERHEAD) {
            Ok(_) => panic!("insufficient fixed-image budget must fail"),
            Err(error) => error,
        };
        assert!(error
            .downcast_ref::<super::super::StagingAcquisitionUncertain>()
            .is_none());
        assert!(!mount.with_extension("dmg").exists());
        assert!(mount.is_dir());
    }

    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    #[test]
    fn attach_spawn_failure_removes_proven_unattached_image() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mount = parent.path().join("mount");
        std::fs::create_dir(&mount).unwrap();
        std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o700)).unwrap();
        let identity = metadata_identity(&std::fs::metadata(&mount).unwrap());
        FAIL_STAGING_ATTACH_SPAWN.with(|fail| fail.set(true));
        let error = match StagingVolume::create(&mount, 64 * 1024 * 1024) {
            Ok(_) => panic!("injected attach spawn failure must fail acquisition"),
            Err(error) => error,
        };
        let spawn = error
            .downcast_ref::<StagingToolNotStarted>()
            .expect("failure must come from the real Command::spawn");
        assert_eq!(spawn.0.raw_os_error(), Some(libc::EAGAIN));
        assert!(error
            .downcast_ref::<super::super::StagingAcquisitionUncertain>()
            .is_none());
        assert!(!mount.with_extension("dmg").exists());
        assert_eq!(
            metadata_identity(&std::fs::metadata(&mount).unwrap()),
            identity
        );
        assert!(std::fs::read_dir(&mount).unwrap().next().is_none());
    }

    fn volume() -> (tempfile::TempDir, StagingVolume) {
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mount = parent.path().join("mount");
        std::fs::create_dir(&mount).unwrap();
        std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o700)).unwrap();
        let volume = StagingVolume::create(&mount, 64 * 1024 * 1024).unwrap();
        (parent, volume)
    }

    fn success(command: &mut Command) -> Vec<u8> {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn fixed_image_bounds_writes_and_rejects_escape() {
        let (parent, mut volume) = volume();
        let image_length = volume
            .image_file
            .as_ref()
            .unwrap()
            .metadata()
            .unwrap()
            .len();
        let outside = parent.path().join("outside");
        std::fs::write(&outside, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, volume.root().join("escape")).unwrap();
        let mut write = volume.command("/bin/sh");
        write
            .arg("-c")
            .arg("printf allowed > inside; printf denied > escape");
        assert!(!write.output().unwrap().status.success());
        assert_eq!(
            std::fs::read(volume.root().join("inside")).unwrap(),
            b"allowed"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        let socket = parent.path().join("host.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut connect = volume.command("/usr/bin/nc");
        connect.args(["-z", "-w", "1", "-U"]).arg(&socket);
        assert!(!connect.output().unwrap().status.success());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let mut fill = volume.command("/bin/dd");
        fill.args(["if=/dev/zero", "of=full", "bs=1048576", "count=64"]);
        assert!(!fill.output().unwrap().status.success());
        volume.verify_mounted().unwrap();
        let metadata = volume.image_file.as_ref().unwrap().metadata().unwrap();
        assert_eq!(metadata.len(), image_length);
        assert!(metadata.blocks() * 512 <= 64 * 1024 * 1024);
        let image = volume.image.clone();
        volume.close().unwrap();
        assert!(!image.exists());
        volume.close().unwrap();
    }

    #[test]
    fn null_device_rdwr_preserves_host_write_boundary() {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::FileTypeExt as _;
        const FIXTURE: &str = "_AOE_INTERNAL_MACOS_STAGING_NULL_FIXTURE";
        if let Some(outside) = std::env::var_os(FIXTURE) {
            let mut null = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .unwrap();
            // SAFETY: F_GETFL only examines the live descriptor.
            assert_eq!(
                unsafe { libc::fcntl(null.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
                libc::O_RDWR
            );
            assert!(null.metadata().unwrap().file_type().is_char_device());
            null.write_all(b"discarded").unwrap();
            assert_eq!(null.read(&mut [0u8; 1]).unwrap(), 0);
            for path in [
                std::path::Path::new(&outside),
                std::path::Path::new("/dev/zero"),
            ] {
                let error = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap_err();
                assert_eq!(error.raw_os_error(), Some(libc::EPERM), "{path:?}");
            }
            println!("null O_RDWR probe completed");
            return;
        }
        let (parent, mut volume) = volume();
        let outside = parent.path().join("host-canary");
        std::fs::write(&outside, b"unchanged").unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut child = volume.command(executable.to_str().unwrap());
        let module = module_path!().split_once("::").unwrap().1;
        child
            .args([
                "--exact",
                &format!("{module}::null_device_rdwr_preserves_host_write_boundary"),
                "--nocapture",
            ])
            .env(FIXTURE, &outside);
        let output = staging_tool(&mut child).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("null O_RDWR probe completed"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        volume.close().unwrap();
    }

    #[test]
    fn readonly_host_fcntls_are_denied() {
        use std::os::fd::AsRawFd as _;
        use std::os::macos::fs::MetadataExt as _;
        const FIXTURE: &str = "_AOE_INTERNAL_MACOS_STAGING_FCNTL_FIXTURE";
        const FILES: [&str; 3] = ["canary", "donor", "receiver"];
        if let Some(fixture) = std::env::var_os(FIXTURE) {
            let files = FILES.map(|name| {
                std::fs::File::open(std::path::Path::new(&fixture).join(name)).unwrap()
            });
            for file in &files {
                // SAFETY: F_GETFL only examines the live descriptor.
                assert_eq!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
                    libc::O_RDONLY
                );
            }
            let mut attributes = libc::attrlist {
                bitmapcount: 5,
                reserved: 0,
                commonattr: libc::ATTR_CMN_GEN_COUNT,
                volattr: 0,
                dirattr: 0,
                fileattr: 0,
                forkattr: 0,
            };
            let mut generation = [0u32; 2];
            // SAFETY: both writable buffers match the requested generation
            // attribute and remain live for the call.
            let result = unsafe {
                libc::fgetattrlist(
                    files[0].as_raw_fd(),
                    (&mut attributes as *mut libc::attrlist).cast(),
                    generation.as_mut_ptr().cast(),
                    std::mem::size_of_val(&generation),
                    libc::FSOPT_ATTR_CMN_EXTENDED,
                )
            };
            assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
            assert_eq!(generation[0] as usize, std::mem::size_of_val(&generation));
            for (descriptor, selector, argument) in [
                (files[0].as_raw_fd(), 80, generation[1]),
                (files[1].as_raw_fd(), 110, files[2].as_raw_fd() as u32),
            ] {
                // SAFETY: F_MAKECOMPRESSED receives the queried generation;
                // F_TRANSFEREXTENTS receives the live receiver descriptor.
                let result = unsafe { libc::fcntl(descriptor, selector, argument) };
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    result, -1,
                    "mutating fcntl {selector} unexpectedly succeeded"
                );
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::EPERM),
                    "mutating fcntl {selector} must fail at the sandbox boundary"
                );
            }
            println!("read-only fcntl probes completed");
            return;
        }
        let (parent, mut volume) = volume();
        let fixture = parent.path().join("fcntl-fixture");
        std::fs::create_dir(&fixture).unwrap();
        std::fs::write(fixture.join(FILES[0]), b"outside sandbox canary\n").unwrap();
        std::fs::write(fixture.join(FILES[2]), []).unwrap();
        // Keep the donor open in the parent so closing the child's read-only
        // descriptor cannot discard preallocated extents before verification.
        let donor = std::fs::File::create(fixture.join(FILES[1])).unwrap();
        let mut allocation = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: 65536,
            fst_bytesalloc: 0,
        };
        // SAFETY: F_PREALLOCATE receives the live allocation structure.
        assert_eq!(
            unsafe { libc::fcntl(donor.as_raw_fd(), libc::F_PREALLOCATE, &mut allocation) },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let snapshot = || {
            FILES.map(|name| {
                let path = fixture.join(name);
                let metadata = std::fs::metadata(&path).unwrap();
                (
                    std::fs::read(path).unwrap(),
                    metadata.blocks(),
                    metadata.st_flags(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            })
        };
        let before = snapshot();
        assert!(before[1].1 > 0, "extent donor must own allocated blocks");
        let executable = std::env::current_exe().unwrap();
        let mut child = volume.command(executable.to_str().unwrap());
        let module = module_path!().split_once("::").unwrap().1;
        child
            .args([
                "--exact",
                &format!("{module}::readonly_host_fcntls_are_denied"),
                "--nocapture",
            ])
            .env(FIXTURE, &fixture);
        let output = staging_tool(&mut child).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("read-only fcntl probes completed"));
        assert_eq!(
            snapshot(),
            before,
            "sandbox must preserve host bytes and metadata"
        );
        drop(donor);
        volume.close().unwrap();
    }

    #[test]
    fn changed_image_identity_retains_recovery_evidence() {
        let (_parent, mut volume) = volume();
        let retained = volume.image.with_extension("retained.dmg");
        std::fs::rename(&volume.image, &retained).unwrap();
        std::fs::write(&volume.image, b"unrelated").unwrap();
        assert!(volume.close().is_err());
        assert!(retained.exists());
        assert_eq!(std::fs::read(&volume.image).unwrap(), b"unrelated");
        std::fs::remove_file(&volume.image).unwrap();
        std::fs::rename(&retained, &volume.image).unwrap();
        volume.close().unwrap();
    }

    #[test]
    fn git_filter_cannot_write_host() {
        let (parent, mut volume) = volume();
        let repository = parent.path().join("source");
        std::fs::create_dir(&repository).unwrap();
        success(Command::new("git").arg("init").arg(&repository));
        std::fs::write(
            repository.join(".gitattributes"),
            b"payload filter=boundary\n",
        )
        .unwrap();
        std::fs::write(repository.join("payload"), b"payload\n").unwrap();
        success(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(["add", "."]),
        );
        success(Command::new("git").arg("-C").arg(&repository).args([
            "-c",
            "user.name=Sandbox Test",
            "-c",
            "user.email=sandbox@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture",
        ]));
        let checkout = volume.root().join("checkout");
        success(
            volume
                .git_command()
                .arg("clone")
                .arg("--no-local")
                .arg("--no-checkout")
                .arg(&repository)
                .arg(&checkout),
        );
        let outside = parent.path().join("must-not-exist");
        // Git itself supplies the filter's shell. This exercises inherited
        // confinement, rather than trusting checkout hooks to remain benign.
        let filter = format!(
            "printf denied > '{}'; printf ran > filter-ran; cat",
            outside.display()
        );
        success(
            volume
                .git_command()
                .arg("-C")
                .arg(&checkout)
                .arg("-c")
                .arg(format!("filter.boundary.smudge={filter}"))
                .args(["checkout", "--force", "HEAD"]),
        );
        assert!(!outside.exists());
        assert_eq!(std::fs::read(checkout.join("filter-ran")).unwrap(), b"ran");
        assert_eq!(
            std::fs::read(checkout.join("payload")).unwrap(),
            b"payload\n"
        );
        volume.close().unwrap();
    }

    #[test]
    fn cloexec_preserves_spawn_error_pipe() {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::process::CommandExt as _;
        let parent = tempfile::tempdir().unwrap();
        let marker = std::fs::File::create(parent.path().join("inherited")).unwrap();
        // SAFETY: duplicate is newly owned and closed by its File wrapper.
        let duplicate = unsafe { libc::fcntl(marker.as_raw_fd(), libc::F_DUPFD, 128) };
        assert!(duplicate >= 128);
        let duplicate = unsafe { std::fs::File::from_raw_fd(duplicate) };
        let ceiling = staging_fd_ceiling().unwrap();
        let mut probe = Command::new("/bin/sh");
        probe
            .arg("-c")
            .arg(format!("test ! -e /dev/fd/{}", duplicate.as_raw_fd()));
        unsafe {
            probe.pre_exec(move || staging_cloexec_descriptors(ceiling));
        }
        success(&mut probe);
        let mut missing = Command::new("/aoe-missing-executable-for-cloexec-test");
        unsafe {
            missing.pre_exec(move || staging_cloexec_descriptors(ceiling));
        }
        assert_eq!(
            missing.spawn().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let mut rejected = Command::new("/usr/bin/true");
        unsafe {
            rejected.pre_exec(move || {
                staging_cloexec_descriptors(ceiling)?;
                Err(std::io::Error::from_raw_os_error(libc::EACCES))
            });
        }
        assert_eq!(
            rejected.spawn().unwrap_err().raw_os_error(),
            Some(libc::EACCES)
        );
    }

    #[test]
    fn sandbox_https_dns_tls() {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        use std::time::{Duration, Instant};
        struct Server {
            child: std::process::Child,
            stderr: std::process::ChildStderr,
            diagnostic: Vec<u8>,
            truncated: bool,
        }
        impl Server {
            fn drain_stderr(&mut self) {
                let mut buffer = [0u8; 1024];
                for _ in 0..16 {
                    match self.stderr.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let retained = count.min(8192 - self.diagnostic.len());
                            self.diagnostic.extend_from_slice(&buffer[..retained]);
                            self.truncated |= retained < count;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("reading local TLS server stderr: {error}"),
                    }
                }
            }

            fn diagnostic(&mut self) -> String {
                let status = self.child.try_wait().unwrap();
                self.drain_stderr();
                format!(
                    "status={status:?}, stderr={}{}",
                    String::from_utf8_lossy(&self.diagnostic),
                    if self.truncated { " [truncated]" } else { "" }
                )
            }
        }
        impl Drop for Server {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        let (parent, mut volume) = volume();
        let certificate = parent.path().join("certificate.pem");
        let key = parent.path().join("key.pem");
        let config = parent.path().join("openssl.cnf");
        std::fs::write(
            &config,
            b"[req]\ndistinguished_name=dn\nx509_extensions=ext\n\
            [dn]\n[ext]\nsubjectAltName=DNS:localhost\nbasicConstraints=critical,CA:TRUE\n\
            keyUsage=digitalSignature,keyEncipherment,keyCertSign\n",
        )
        .unwrap();
        let mut generate = Command::new("/usr/bin/openssl");
        generate
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-config",
            ])
            .arg(&config)
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&certificate);
        staging_tool(&mut generate).unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let serve = r#"import socket, ssl, sys
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[2], sys.argv[3])
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind(("127.0.0.1", int(sys.argv[1])))
    listener.listen(8)
    while True:
        connection, _ = listener.accept()
        connection.settimeout(10)
        try:
            with context.wrap_socket(connection, server_side=True) as stream:
                request = b""
                while b"\r\n\r\n" not in request:
                    data = stream.recv(4096)
                    if not data:
                        break
                    request += data
                    if len(request) > 8192:
                        raise ValueError("fixture request too large")
                stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        except (ssl.SSLError, ConnectionError, TimeoutError):
            connection.close()
"#;
        let mut child = Command::new("python3")
            .args(["-B", "-c", serve])
            .arg(address.port().to_string())
            .arg(&certificate)
            .arg(&key)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut server = Server {
            child,
            stderr,
            diagnostic: Vec::new(),
            truncated: false,
        };
        // Drain diagnostics without allowing the child to block on a full pipe.
        let fd = server.stderr.as_raw_fd();
        // SAFETY: the live stderr pipe belongs to server.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
            "{}",
            std::io::Error::last_os_error()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            server.drain_stderr();
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "local TLS server exited: {}",
                server.diagnostic()
            );
            if std::net::TcpStream::connect(address).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "local TLS server did not start: {}",
                server.diagnostic()
            );
            // Wait for diagnostics/exit or the next readiness probe, not a
            // presumed startup duration. The actual request runs only once.
            let mut event = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: event points to one initialized, live poll descriptor.
            let result = unsafe { libc::poll(&mut event, 1, 10) };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
            }
        }
        let mut request = volume.command("/usr/bin/curl");
        request
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--max-time",
                "10",
                "--noproxy",
                "*",
                "--cacert",
            ])
            .arg(&certificate)
            .arg(format!("https://localhost:{}/", address.port()));
        // localhost exercises getaddrinfo without depending on public DNS.
        // Certificate verification stays enabled against the private fixture.
        let output = request.output().unwrap();
        assert!(
            output.status.success(),
            "curl failed ({}): {}\nlocal TLS server: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            server.diagnostic()
        );
        assert_eq!(
            output.stdout,
            b"ok",
            "unexpected TLS response: {}",
            server.diagnostic()
        );
        volume.close().unwrap();
    }
}

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = true;

const CODEX_PREFERENCES_DOMAIN: &str = "com.openai.codex";

/// Whether CFPreferences holds a Codex managed configuration or requirements
/// value. Forced or not: Codex before openai/codex#46032 read either.
pub(crate) fn codex_managed_preferences_present() -> bool {
    codex_managed_preferences_present_with(|key| {
        preference_value_present(key, CODEX_PREFERENCES_DOMAIN)
    })
}

fn codex_managed_preferences_present_with(present: impl Fn(&str) -> bool) -> bool {
    ["config_toml_base64", "requirements_toml_base64"]
        .into_iter()
        .any(present)
}

fn preference_value_present(key: &str, application: &str) -> bool {
    use core_foundation::base::{CFType, TCFType as _};
    use core_foundation::string::CFString;
    let (key, application) = (CFString::new(key), CFString::new(application));
    // SAFETY: both arguments are live CFStrings.
    let value = unsafe {
        core_foundation_sys::preferences::CFPreferencesCopyAppValue(
            key.as_concrete_TypeRef(),
            application.as_concrete_TypeRef(),
        )
    };
    if value.is_null() {
        return false;
    }
    // SAFETY: the copy rule hands ownership of a non-null result to the wrapper.
    drop(unsafe { CFType::wrap_under_create_rule(value) });
    true
}
/// Prove only the absence of the Linux scope backend on this kernel.
/// Native process, worker and tmux custody must still pass their own guards.
pub(super) fn legacy_scope_inventory_clear(
    known: &[crate::process::worker_registry::ScopeOwnership],
) -> anyhow::Result<bool> {
    use crate::process::worker_registry::ScopeOwnership;
    anyhow::ensure!(
        !known
            .iter()
            .any(|scope| matches!(scope, ScopeOwnership::Owned { .. })),
        "Captured Linux scope authority cannot be recovered on this platform"
    );
    let mut name = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname receives writable storage of the required size.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Reading the native kernel identity");
    }
    // SAFETY: uname succeeded and initializes NUL-terminated sysname.
    let name = unsafe { name.assume_init() };
    let kernel = unsafe { std::ffi::CStr::from_ptr(name.sysname.as_ptr()) };
    anyhow::ensure!(
        kernel.to_bytes() == b"Darwin",
        "Cannot prove the Linux scope backend absent on this kernel"
    );
    Ok(true)
}

#[cfg(test)]
mod legacy_scope_recovery_tests {
    use super::*;
    use crate::process::worker_registry::ScopeOwnership;

    #[test]
    fn native_darwin_proof_does_not_discard_a_captured_linux_unit() {
        assert!(legacy_scope_inventory_clear(&[]).unwrap());
        assert!(legacy_scope_inventory_clear(&[ScopeOwnership::Unscoped]).unwrap());
        assert!(legacy_scope_inventory_clear(&[ScopeOwnership::Owned {
            unit: "aoe-session-old-original.scope".into(),
        }])
        .is_err());
    }
}

pub(super) fn open_read_nonblocking(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

pub(super) fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}
pub(super) fn rename_exclusive(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    rename_with_flags(
        source_dir,
        source,
        destination_dir,
        destination,
        libc::RENAME_EXCL,
    )
}

pub(super) fn rename_exchange(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    rename_with_flags(
        source_dir,
        source,
        destination_dir,
        destination,
        libc::RENAME_SWAP,
    )
}

fn rename_with_flags(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    use nix::NixPath;
    use std::os::fd::AsRawFd;

    let result = source.with_nix_path(|source| {
        destination.with_nix_path(|destination| {
            // SAFETY: both descriptors remain owned for the call and NixPath
            // supplies live NUL-terminated names. Flags select the atomic rename policy.
            nix::errno::Errno::result(unsafe {
                libc::renameatx_np(
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    destination_dir.as_raw_fd(),
                    destination.as_ptr(),
                    flags,
                )
            })
            .map(|_| ())
        })
    })??;
    result.map_err(std::io::Error::from)
}

/// tokio reads the peer through `LOCAL_PEERCRED`, the macOS counterpart of Linux `SO_PEERCRED`.
pub(crate) fn unix_peer_uid(stream: &tokio::net::UnixStream) -> std::io::Result<u32> {
    Ok(stream.peer_cred()?.uid())
}

#[cfg(test)]
mod peer_uid_tests {
    #[tokio::test]
    async fn connected_pair_reports_the_process_uid() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        assert_eq!(super::unix_peer_uid(&a).unwrap(), uid);
        assert_eq!(super::unix_peer_uid(&b).unwrap(), uid);
    }
}
/// Run the child at background priority so a mass file delete does not saturate
/// `fseventsd` and stall the rest of the machine.
pub(super) fn throttle_child(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setpriority` is async-signal-safe and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
            Ok(())
        });
    }
}

pub(super) fn collect_pid_tree(pid: u32) -> Vec<u32> {
    let children_map = build_children_map();
    let mut pids = vec![pid];
    super::collect_descendants_from_map(pid, &children_map, &mut pids);
    pids
}

pub(super) fn build_children_map() -> HashMap<u32, Vec<u32>> {
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();

    let Ok(output) = Command::new("ps").args(["-o", "pid=,ppid=", "-A"]).output() else {
        return children_map;
    };

    if !output.status.success() {
        return children_map;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(child_pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                children_map.entry(ppid).or_default().push(child_pid);
            }
        }
    }

    children_map
}

/// One `ps -A -ww -E -o command=` fork deciding, for each candidate `i`,
/// whether a live process belongs to it: a whitespace-delimited token exactly
/// equals `env_needles[i]` (anchored, matching the `KEY=VAL` env tokens `-E`
/// appends), and the line contains `cmdline_needles[i]` when both are supplied.
/// An executable needle matches an exact argv-token basename. A candidate with
/// one signal uses that one; otherwise every supplied signal must match.
/// `-ww` disables column
/// truncation; `-E` appends each owner-owned process's environment. If a `ps`
/// build rejects `-E`, the call fails closed to all `false` and recovery falls
/// back to the ledger. Best-effort: a failed `ps` yields all `false`.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
) -> Vec<bool> {
    let n = env_needles.len();
    let mut found = vec![false; n];
    let Ok(output) = Command::new("ps")
        .args(["-A", "-ww", "-E", "-o", "command="])
        .output()
    else {
        return found;
    };
    if !output.status.success() {
        return found;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_position = (!env_needles[i].is_empty())
                .then(|| tokens.iter().position(|token| *token == env_needles[i]))
                .flatten();
            let env_hit = env_position.is_some();
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && line.contains(s));
            let command_end = env_position.unwrap_or(tokens.len());
            let executable_hit = executable_needles[i].as_deref().is_some_and(|needle| {
                !needle.is_empty()
                    && tokens[..command_end].iter().any(|token| {
                        std::path::Path::new(token)
                            .file_name()
                            .and_then(|value| value.to_str())
                            == Some(needle)
                    })
            });
            let has_env = !env_needles[i].is_empty();
            let has_cmd = cmdline_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let has_executable = executable_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let matched = (has_env || has_cmd || has_executable)
                && (!has_env || env_hit)
                && (!has_cmd || cmd_hit)
                && (!has_executable || executable_hit);
            if matched {
                found[i] = true;
            }
        }
    }
    found
}

pub(super) fn runner_group_executing(group: u32) -> anyhow::Result<bool> {
    let mut command = Command::new("ps");
    command
        .args(["-A", "-o", "pid=,pgid=,stat="])
        .env("LC_ALL", "C");
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "runner group census",
        1024 * 1024,
    )?;
    for line in std::str::from_utf8(&output)?.lines() {
        let mut fields = line.split_ascii_whitespace();
        fields
            .next()
            .context("missing process PID")?
            .parse::<u32>()?;
        let process_group = fields
            .next()
            .context("missing process group")?
            .parse::<u32>()?;
        let state = fields.next().context("missing process state")?;
        if process_group == group && !state.starts_with('Z') {
            return Ok(true);
        }
    }
    Ok(false)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct AuditToken {
    values: [u32; 8],
}

impl AuditToken {
    fn matches_start_id(&self, value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes.len() == 70
            && bytes.starts_with(b"audit:")
            && self.values.iter().enumerate().all(|(index, word)| {
                (0..8).all(|digit| {
                    let nibble = ((*word >> (28 - digit * 4)) & 15) as usize;
                    bytes[6 + index * 8 + digit] == b"0123456789abcdef"[nibble]
                })
            })
    }
}

unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn task_name_for_pid(
        task: libc::mach_port_t,
        pid: libc::pid_t,
        name: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

fn audit_token(pid: u32) -> anyhow::Result<Option<AuditToken>> {
    let raw_pid = i32::try_from(pid)?;
    anyhow::ensure!(raw_pid > 0, "invalid owned process PID");
    let mut port = 0;
    let task = unsafe { mach_task_self_ };
    let result = unsafe { task_name_for_pid(task, raw_pid, &mut port) };
    if result != 0 {
        if !super::worker::is_pid_alive(pid) {
            return Ok(None);
        }
        if process_membership(pid)?.is_none_or(|info| info.pbi_status == libc::SZOMB) {
            return Ok(None);
        }
        anyhow::bail!("process audit token unavailable: Mach error {result}");
    }
    struct Port(libc::mach_port_t, libc::mach_port_t);
    impl Drop for Port {
        fn drop(&mut self) {
            unsafe {
                mach_port_deallocate(self.0, self.1);
            }
        }
    }
    let _port = Port(task, port);
    let mut token = AuditToken { values: [0; 8] };
    let mut count = 8;
    let result = unsafe { libc::task_info(port, 15, token.values.as_mut_ptr().cast(), &mut count) };
    if result != 0 || count != 8 || token.values[5] != pid {
        if !super::worker::is_pid_alive(pid)
            || process_membership(pid)?.is_none_or(|info| info.pbi_status == libc::SZOMB)
        {
            return Ok(None);
        }
        anyhow::bail!(
            "invalid process audit token: Mach error {result}, count {count}, PID {}",
            token.values[5]
        );
    }
    Ok(Some(token))
}

fn audit_start_id(token: &AuditToken) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(70);
    value.push_str("audit:");
    for word in token.values {
        write!(value, "{word:08x}").unwrap();
    }
    value
}

pub(super) fn process_start_id(pid: u32) -> anyhow::Result<Option<String>> {
    Ok(audit_token(pid)?.as_ref().map(audit_start_id))
}

#[repr(C)]
struct UniqueIdentifierInfo {
    _uuid: [u8; 16],
    unique_id: u64,
    parent_unique_id: u64,
    version: i32,
    _original_parent_version: i32,
    _reserved: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<UniqueIdentifierInfo>() == 56);

#[repr(C)]
struct OwnedProcessInfo {
    bsd: libc::proc_bsdinfo,
    identity: UniqueIdentifierInfo,
}

fn unique_identity(
    pid: u32,
    include_zombies: bool,
) -> anyhow::Result<Option<UniqueIdentifierInfo>> {
    let mut info = std::mem::MaybeUninit::<UniqueIdentifierInfo>::uninit();
    let size = std::mem::size_of::<UniqueIdentifierInfo>() as i32;
    // PROC_PIDUNIQIDENTIFIERINFO includes physical birth independently of exec.
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            17,
            u64::from(include_zombies),
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        let error = std::io::Error::last_os_error();
        if read == 0 && error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        anyhow::bail!("process birth metadata unavailable ({read}/{size}): {error}");
    }
    Ok(Some(unsafe { info.assume_init() }))
}

fn owned_membership(
    pid: u32,
    deadline: std::time::Instant,
) -> anyhow::Result<Option<OwnedProcessInfo>> {
    let Some(initial) = unique_identity(pid, true)? else {
        return Ok(None);
    };
    loop {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "owned ancestry observation expired"
        );
        let Some(before) = unique_identity(pid, true)? else {
            return Ok(None);
        };
        if before.unique_id != initial.unique_id {
            return Ok(None);
        }
        let mut info = std::mem::MaybeUninit::<OwnedProcessInfo>::uninit();
        let size = std::mem::size_of::<OwnedProcessInfo>() as i32;
        // PROC_PIDT_BSDINFOWITHUNIQID reads both structures from the same process.
        let read = unsafe {
            libc::proc_pidinfo(i32::try_from(pid)?, 18, 1, info.as_mut_ptr().cast(), size)
        };
        if read != size {
            let error = std::io::Error::last_os_error();
            if read == 0 && error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            anyhow::bail!("owned ancestry metadata unavailable ({read}/{size}): {error}");
        }
        let info = unsafe { info.assume_init() };
        let Some(after) = unique_identity(pid, true)? else {
            return Ok(None);
        };
        if info.identity.unique_id != initial.unique_id || after.unique_id != initial.unique_id {
            return Ok(None);
        }
        anyhow::ensure!(info.bsd.pbi_pid == pid, "unexpected owned ancestry PID");
        if before.version == info.identity.version && info.identity.version == after.version {
            return Ok(Some(info));
        }
        std::thread::yield_now();
    }
}

fn owned_audit_token(
    pid: u32,
    birth: u64,
    deadline: std::time::Instant,
) -> anyhow::Result<Option<AuditToken>> {
    loop {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "owned execution observation expired"
        );
        let Some(before) = unique_identity(pid, false)? else {
            return Ok(None);
        };
        if before.unique_id != birth {
            return Ok(None);
        }
        let observed = audit_token(pid);
        let Some(after) = unique_identity(pid, false)? else {
            return Ok(None);
        };
        if after.unique_id != birth {
            return Ok(None);
        }
        if before.version != after.version {
            std::thread::yield_now();
            continue;
        }
        let token = observed?.context("live owned process audit token unavailable")?;
        if token.values[7] == after.version as u32 {
            return Ok(Some(token));
        }
        std::thread::yield_now();
    }
}

fn signal_audit_token(mut token: AuditToken, signal: i32) -> std::io::Result<()> {
    type Signal = unsafe extern "C" fn(*mut AuditToken, i32) -> i32;
    static FUNCTION: std::sync::LazyLock<Option<Signal>> = std::sync::LazyLock::new(|| unsafe {
        let library = libc::dlopen(
            c"/usr/lib/libproc.dylib".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return None;
        }
        let function = libc::dlsym(library, c"proc_signal_with_audittoken".as_ptr());
        if function.is_null() {
            libc::dlclose(library);
            return None;
        }
        // The cached function keeps its library loaded for the process lifetime.
        Some(std::mem::transmute::<*mut libc::c_void, Signal>(function))
    });
    let function = FUNCTION.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "incarnation-bound signalling unavailable on this macOS version",
        )
    })?;
    let result = unsafe { function(&mut token, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(result))
    }
}

pub(super) struct ProcessHandle {
    initial_token: AuditToken,
    owned_birth: Option<u64>,
}

/// False once `pid` is gone, or exited and not yet reaped (SZOMB).
pub(super) fn process_executing(pid: u32) -> anyhow::Result<bool> {
    Ok(process_membership(pid)?.is_some_and(|info| info.pbi_status != libc::SZOMB))
}

impl ProcessHandle {
    pub(super) fn capture(identity: &super::PrimeProcessIdentity) -> anyhow::Result<Option<Self>> {
        anyhow::ensure!(
            identity.start_id.starts_with("audit:"),
            "process lacks incarnation-bound signal authority"
        );
        let Some(token) = audit_token(identity.pid)? else {
            return Ok(None);
        };
        if !token.matches_start_id(&identity.start_id) {
            return Ok(None);
        }
        Ok(Some(Self {
            initial_token: token,
            owned_birth: None,
        }))
    }

    pub(super) fn capture_owned(
        identity: &super::PrimeProcessIdentity,
        root: &super::OwnedProcessGroup,
    ) -> anyhow::Result<Option<Self>> {
        root.exited()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let Some(mut child) = owned_membership(identity.pid, deadline)? else {
            return Ok(None);
        };
        if child.bsd.pbi_status == libc::SZOMB {
            return Ok(None);
        }
        let birth = child.identity.unique_id;
        let root_birth =
            unique_identity(root.id(), true)?.context("unreaped command birth unavailable")?;
        loop {
            if child.bsd.pbi_ppid == root.id() {
                anyhow::ensure!(
                    child.identity.parent_unique_id == root_birth.unique_id,
                    "owned root parent birth changed",
                );
                break;
            }
            anyhow::ensure!(
                child.bsd.pbi_ppid > 1 && child.bsd.pbi_ppid != child.bsd.pbi_pid,
                "process {} lacks owned command ancestry",
                identity.pid,
            );
            let parent = owned_membership(child.bsd.pbi_ppid, deadline)?
                .context("owned parent exited before ancestry proof")?;
            anyhow::ensure!(
                child.identity.parent_unique_id == parent.identity.unique_id,
                "owned parent birth changed",
            );
            let current = unique_identity(child.bsd.pbi_pid, true)?
                .context("owned ancestor exited before ancestry proof")?;
            anyhow::ensure!(
                current.unique_id == child.identity.unique_id,
                "owned ancestor PID was reused",
            );
            child = parent;
        }
        let Some(token) = owned_audit_token(identity.pid, birth, deadline)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            initial_token: token,
            owned_birth: Some(birth),
        }))
    }

    pub(super) fn exited(&self) -> anyhow::Result<bool> {
        Ok(audit_token(self.initial_token.values[5])?
            .is_none_or(|current| current.values[7] != self.initial_token.values[7]))
    }

    pub(super) fn physically_exited(&self) -> anyhow::Result<bool> {
        let birth = self
            .owned_birth
            .context("physical observation lacks owned birth")?;
        Ok(unique_identity(self.initial_token.values[5], false)?
            .is_none_or(|current| current.unique_id != birth))
    }

    pub(super) fn signal_owned(
        &self,
        signal: i32,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        let birth = self
            .owned_birth
            .context("owned signal lacks physical birth")?;
        let pid = self.initial_token.values[5];
        loop {
            let Some(token) = owned_audit_token(pid, birth, deadline)? else {
                return Ok(());
            };
            match signal_audit_token(token, signal) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let Some(current) = unique_identity(pid, false)? else {
                        return Ok(());
                    };
                    if current.unique_id != birth {
                        return Ok(());
                    }
                    if current.version as u32 != token.values[7] {
                        continue;
                    }
                    return Err(error).context("signalling owned physical birth");
                }
            }
        }
    }

    pub(super) fn signal(&self, signal: i32) -> anyhow::Result<()> {
        match signal_audit_token(self.initial_token, signal) {
            Ok(()) => Ok(()),
            Err(error)
                if error.raw_os_error() == Some(libc::ESRCH)
                    || (error.raw_os_error() == Some(libc::EPERM) && self.exited()?) =>
            {
                Ok(())
            }
            Err(error) => Err(error).context("signalling owned audit token"),
        }
    }
}

pub(super) fn runner_group_identities(
    group: u32,
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    let mut command = Command::new("ps");
    command.args(["-A", "-o", "pid=,pgid=,stat="]);
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "runner group identities",
        1024 * 1024,
    )?;
    let mut identities = Vec::new();
    for line in std::str::from_utf8(&output)?.lines() {
        let mut fields = line.split_whitespace();
        let pid = fields.next().context("missing process PID")?.parse()?;
        let pgrp = fields
            .next()
            .context("missing process group")?
            .parse::<u32>()?;
        let zombie = fields
            .next()
            .context("missing process state")?
            .starts_with('Z');
        if pgrp == group && !zombie {
            if let Some(start_id) = process_start_id(pid)? {
                if process_group_id(pid)? == Some(group)
                    && process_start_id(pid)?.as_ref() == Some(&start_id)
                {
                    identities.push(super::PrimeProcessIdentity { pid, start_id });
                }
            }
        }
    }
    Ok(identities)
}
pub(super) fn process_group_id(pid: u32) -> anyhow::Result<Option<u32>> {
    let group = unsafe { libc::getpgid(i32::try_from(pid)?) };
    if group >= 0 {
        return Ok(Some(group as u32));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(error.into())
    }
}

fn process_membership(pid: u32) -> anyhow::Result<Option<libc::proc_bsdinfo>> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        anyhow::bail!("process membership unavailable: {error}");
    }
    let info = unsafe { info.assume_init() };
    anyhow::ensure!(info.pbi_pid == pid, "unexpected process membership PID");
    Ok(Some(info))
}

pub(super) fn foreign_runner_group_absent(pid: u32) -> anyhow::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return Err(std::io::Error::last_os_error())
            .context("runner short process metadata unavailable");
    }
    let info = unsafe { info.assume_init() };
    anyhow::ensure!(info.pbsi_pid == pid, "unexpected runner metadata PID");
    let uid = unsafe { libc::geteuid() };
    let ruid = unsafe { libc::getuid() };
    Ok((info.pbsi_uid != uid || info.pbsi_ruid != ruid) && !runner_group_executing(pid)?)
}
pub(super) fn process_tree_samples(
    root: &super::PrimeProcessIdentity,
) -> anyhow::Result<Vec<super::PrimeProcessSample>> {
    process_tree_samples_with_root(root, false)
}

pub(super) fn owned_process_tree_samples(
    root: &super::OwnedProcessGroup,
) -> anyhow::Result<Vec<super::PrimeProcessSample>> {
    process_tree_samples_with_root(
        &super::PrimeProcessIdentity {
            pid: root.id(),
            start_id: String::new(),
        },
        true,
    )
}

fn process_tree_samples_with_root(
    root: &super::PrimeProcessIdentity,
    owned_root: bool,
) -> anyhow::Result<Vec<super::PrimeProcessSample>> {
    let mut command = Command::new("ps");
    command.args(["-A", "-o", "pid=,ppid=,stat="]);
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "tool process census",
        1024 * 1024,
    )?;
    let mut candidates = Vec::new();
    if owned_root {
        candidates.push(super::PrimeProcessSample {
            identity: root.clone(),
            parent: 0,
            marked: false,
        });
    }
    for line in std::str::from_utf8(&output)?.lines() {
        let mut fields = line.split_ascii_whitespace();
        let pid = fields.next().context("missing process PID")?.parse()?;
        let parent = fields.next().context("missing process parent")?.parse()?;
        if owned_root && pid == root.pid {
            continue;
        }
        if !fields
            .next()
            .context("missing process state")?
            .starts_with('Z')
        {
            candidates.push(super::PrimeProcessSample {
                identity: super::PrimeProcessIdentity {
                    pid,
                    start_id: if pid == root.pid {
                        root.start_id.clone()
                    } else {
                        String::new()
                    },
                },
                parent,
                marked: false,
            });
        }
    }
    let selected = super::prime_selection_mask(&candidates, std::slice::from_ref(root));
    let mut samples = Vec::new();
    for (mut sample, selected) in candidates.into_iter().zip(selected) {
        if !selected {
            continue;
        }
        if owned_root && sample.identity.pid == root.pid {
            samples.push(sample);
            continue;
        }
        let pid = sample.identity.pid;
        if owned_root {
            if let Some(membership) = process_membership(pid)? {
                if membership.pbi_status != libc::SZOMB {
                    sample.parent = membership.pbi_ppid;
                    samples.push(sample);
                }
            }
            continue;
        }
        let Some(token) = audit_token(pid)? else {
            continue;
        };
        let membership = process_membership(pid)?.context("tool process membership unavailable")?;
        let after = audit_token(pid)?;
        anyhow::ensure!(
            after == Some(token),
            "tool process incarnation changed during census"
        );
        sample.identity.start_id = audit_start_id(&token);
        sample.parent = membership.pbi_ppid;
        samples.push(sample);
    }
    if !owned_root {
        anyhow::ensure!(
            samples.iter().any(|sample| sample.identity == *root),
            "tool root omitted from census"
        );
    }
    Ok(samples)
}

pub(super) fn prime_processes(
    marker: &str,
    socket: &std::path::Path,
    known: &[super::PrimeProcessIdentity],
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    let mut command = Command::new("ps");
    command
        .args(["-A", "-o", "pid=,ppid=,stat=,lstart="])
        .env("LC_ALL", "C")
        .env("LC_TIME", "C")
        .env("LANG", "C")
        .env("TZ", "UTC");
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "Prime process census",
        1024 * 1024,
    )?;
    let mut samples = Vec::new();
    let mut bytes = Vec::new();
    let mut legacy_observed = Vec::new();
    let mut known_zombies = Vec::new();
    for line in std::str::from_utf8(&output)?.lines() {
        let mut rest = line;
        let mut take = || -> anyhow::Result<&str> {
            let (value, tail) = rest
                .trim_start()
                .split_once(char::is_whitespace)
                .context("incomplete process census row")?;
            rest = tail;
            Ok(value)
        };
        let pid = take()?.parse::<u32>()?;
        let parent = take()?.parse::<u32>()?;
        let zombie = take()?.starts_with('Z');
        if zombie && known.iter().any(|identity| identity.pid == pid) {
            known_zombies.push(pid);
        }
        if zombie || pid == std::process::id() {
            continue;
        }
        let start = rest.trim();
        anyhow::ensure!(!start.is_empty(), "missing process start identity");
        let marked = match prime_launch_marker(pid, marker, socket, &mut bytes) {
            Ok(marked) => marked,
            Err(error) if known.iter().any(|identity| identity.pid == pid) => return Err(error),
            Err(_) => false,
        };
        samples.push(super::PrimeProcessSample {
            identity: super::PrimeProcessIdentity {
                pid,
                start_id: format!("ps:{start}"),
            },
            parent,
            marked,
        });
    }
    let candidate_known: Vec<_> = samples
        .iter()
        .filter(|sample| {
            known
                .iter()
                .any(|identity| identity.pid == sample.identity.pid)
        })
        .map(|sample| sample.identity.clone())
        .collect();
    let candidates = super::prime_selection_mask(&samples, &candidate_known);
    let mut attested = Vec::new();
    for (mut sample, candidate) in samples.into_iter().zip(candidates) {
        if !candidate {
            continue;
        }
        let pid = sample.identity.pid;
        let Some(token) = audit_token(pid)? else {
            continue;
        };
        let Some(membership) = process_membership(pid)? else {
            continue;
        };
        let Some(after) = audit_token(pid)? else {
            continue;
        };
        anyhow::ensure!(
            after == token,
            "owned process incarnation changed during census"
        );
        if !sample.marked && known.iter().any(|known| known == &sample.identity) {
            legacy_observed.push(super::PrimeProcessIdentity {
                pid,
                start_id: audit_start_id(&token),
            });
        }
        sample.identity.start_id = audit_start_id(&token);
        sample.parent = membership.pbi_ppid;
        attested.push(sample);
    }
    let samples = attested;
    for identity in known {
        if !samples
            .iter()
            .any(|sample| sample.identity.pid == identity.pid)
        {
            anyhow::ensure!(
                known_zombies.contains(&identity.pid) || !super::worker::is_pid_alive(identity.pid),
                "owned Prime process {} omitted from census",
                identity.pid
            );
        }
    }
    let owned = super::select_stable_prime_processes(samples, known)?;
    for identity in legacy_observed {
        anyhow::ensure!(
            owned.contains(&identity)
                || process_start_id(identity.pid)?.as_deref() != Some(&identity.start_id),
            "legacy worker birth is unproven; retain ownership until external settlement"
        );
    }
    Ok(owned)
}

fn prime_launch_marker(
    pid: u32,
    marker: &str,
    socket: &std::path::Path,
    bytes: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    let pid = i32::try_from(pid)?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut length = 0usize;
    let queried = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if queried != 0 {
        return Err(std::io::Error::last_os_error()).context("reading process argument size");
    }
    anyhow::ensure!(
        (4..=256 * 1024).contains(&length),
        "invalid process argument size"
    );
    bytes.clear();
    bytes.reserve(length);
    let capacity = bytes.capacity();
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error()).context("reading process arguments");
    }
    anyhow::ensure!(
        (4..=capacity).contains(&length),
        "invalid process argument result"
    );
    // sysctl initialized exactly length bytes on success.
    unsafe {
        bytes.set_len(length);
    }
    let count = i32::from_ne_bytes(bytes[..4].try_into()?);
    anyhow::ensure!(
        (0..=4096).contains(&count),
        "invalid process argument count"
    );
    let mut tail = &bytes[4..];
    let executable_end = tail
        .iter()
        .position(|byte| *byte == 0)
        .context("unterminated process executable")?;
    tail = &tail[executable_end + 1..];
    while tail.first() == Some(&0) {
        tail = &tail[1..];
    }
    let mut routed = false;
    for _ in 0..count {
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .context("unterminated process argument")?;
        routed |= &tail[..end] == socket.as_os_str().as_encoded_bytes();
        tail = &tail[end + 1..];
    }
    Ok(routed
        || tail
            .split(|byte| *byte == 0)
            .any(|entry| entry == marker.as_bytes()))
}

/// Sample system memory via `sysctl` and `vm_stat`, matching this module's
/// existing shell-out convention. Populates total/available RAM and the native
/// memory-pressure level; PSI has no macOS analogue and stays `None`.
pub(super) fn sample_memory() -> super::metrics::MemorySample {
    // A vm_stat failure alone would otherwise read as a false 100% used.
    let (total_bytes, available_bytes) = match (
        sysctl_u64("hw.memsize"),
        read_vm_stat().and_then(|s| parse_vm_stat_available(&s)),
    ) {
        (Some(total), Some(available)) => (total, available),
        _ => (0, 0),
    };

    super::metrics::MemorySample {
        total_bytes,
        available_bytes,
        psi_mem_some_avg10: None,
        psi_io_some_avg10: None,
        macos_pressure_level: sysctl_u64("kern.memorystatus_vm_pressure_level").map(|v| v as u8),
    }
}

pub(super) fn sample_system() -> super::metrics::SystemReading {
    let cpu = Command::new("ps")
        .args(["-A", "-o", "%cpu="])
        .output()
        .ok()
        .map(|output| {
            let total_percent: f64 = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<f64>().ok())
                .sum();
            let cpus = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1);
            (total_percent / 100.0 / cpus as f64).clamp(0.0, 1.0)
        });
    let load = sysctl_string("vm.loadavg").and_then(|value| {
        let values: Vec<f64> = value
            .replace(['{', '}'], "")
            .split_whitespace()
            .filter_map(|part| part.parse().ok())
            .collect();
        (values.len() >= 3).then(|| [values[0], values[1], values[2]])
    });
    let swap = sysctl_string("vm.swapusage")
        .map(|value| parse_swap(&value))
        .unwrap_or((0, 0));
    (None, cpu, load, swap)
}

pub(super) fn process_snapshot() -> Vec<super::metrics::ProcessRecord> {
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time=,lstart="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_record)
        .collect()
}

fn parse_process_record(line: &str) -> Option<super::metrics::ProcessRecord> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let started_at = fields.get(4..)?;
    if started_at.is_empty() {
        return None;
    }
    Some(super::metrics::ProcessRecord {
        pid: fields.first()?.parse().ok()?,
        ppid: fields.get(1)?.parse().ok()?,
        rss_bytes: fields.get(2)?.parse::<u64>().ok()?.saturating_mul(1024),
        cpu_seconds: parse_ps_time(fields.get(3)?)?,
        start_id: stable_process_start_id(started_at),
    })
}

fn stable_process_start_id(fields: &[&str]) -> u64 {
    fields
        .iter()
        .flat_map(|field| field.bytes().chain(std::iter::once(0)))
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn parse_swap(value: &str) -> (u64, u64) {
    let parse = |label: &str| {
        let raw = value
            .split_whitespace()
            .skip_while(|part| *part != label)
            .nth(2)?;
        let (number, multiplier) = if let Some(number) = raw.strip_suffix('G') {
            (number, 1u64 << 30)
        } else if let Some(number) = raw.strip_suffix('M') {
            (number, 1u64 << 20)
        } else if let Some(number) = raw.strip_suffix('K') {
            (number, 1u64 << 10)
        } else {
            (raw, 1)
        };
        Some((number.parse::<f64>().ok()? * multiplier as f64) as u64)
    };
    (parse("total").unwrap_or(0), parse("used").unwrap_or(0))
}

fn parse_ps_time(value: &str) -> Option<f64> {
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<f64>().ok()?, clock)
    } else {
        (0.0, value)
    };
    let parts: Vec<f64> = clock
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes * 60.0 + seconds,
        [hours, minutes, seconds] => hours * 3600.0 + minutes * 60.0 + seconds,
        _ => return None,
    };
    Some(days * 86_400.0 + seconds)
}

fn sysctl_string(key: &str) -> Option<String> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn sysctl_u64(key: &str) -> Option<u64> {
    sysctl_string(key)?.parse().ok()
}

fn read_vm_stat() -> Option<String> {
    let out = Command::new("vm_stat").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// (free + inactive) pages; an approximation, so the band uses the native pressure level.
fn parse_vm_stat_available(vm_stat: &str) -> Option<u64> {
    let page_size = parse_vm_stat_page_size(vm_stat)?;
    let free = parse_vm_stat_pages(vm_stat, "Pages free")?;
    let inactive = parse_vm_stat_pages(vm_stat, "Pages inactive")?;
    Some((free + inactive).saturating_mul(page_size))
}

fn parse_vm_stat_page_size(vm_stat: &str) -> Option<u64> {
    let line = vm_stat.lines().next()?;
    let after = line.split("page size of").nth(1)?;
    after.split_whitespace().next()?.parse().ok()
}

fn parse_vm_stat_pages(vm_stat: &str, key: &str) -> Option<u64> {
    for line in vm_stat.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.trim().trim_end_matches('.').parse().ok();
    }
    None
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    const VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                              123456.
Pages active:                            654321.
Pages inactive:                          200000.
Pages speculative:                        10000.
Pages wired down:                        300000.
";

    #[test]
    fn test_parse_vm_stat_available() {
        assert_eq!(parse_vm_stat_page_size(VM_STAT), Some(16384));
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages free"), Some(123456));
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages inactive"), Some(200000));
        assert_eq!(
            parse_vm_stat_available(VM_STAT),
            Some((123456 + 200000) * 16384)
        );
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages nonexistent"), None);
    }

    #[test]
    fn process_record_parses_stable_identity_and_cpu_time() {
        let time_cases = [
            ("03:04", Some(184.0)),
            ("02:03:04", Some(7_384.0)),
            ("1-02:03:04", Some(93_784.0)),
            ("1-02:x:04", None),
            ("1-02:03:bad", None),
        ];
        for (value, expected) in time_cases {
            assert_eq!(parse_ps_time(value), expected, "CPU time {value}");
        }

        let line = "42 1 512 1-02:03:04 Thu Aug 13 12:34:56 2026";
        let record = parse_process_record(line).unwrap();
        assert_eq!(record.cpu_seconds, 93_784.0);
        assert_ne!(record.start_id, 0);
        assert_eq!(
            record.start_id,
            parse_process_record(line).unwrap().start_id
        );
    }
}

/// `kern.boottime` shifts on clock steps; the session UUID does not.
pub(super) fn boot_id() -> Option<String> {
    let mut uuid = [0u8; 37];
    let mut length = uuid.len();
    // SAFETY: the name is NUL-terminated; buffer and length match its capacity.
    if unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            uuid.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let value = std::ffi::CStr::from_bytes_with_nul(uuid.get(..length)?)
        .ok()?
        .to_str()
        .ok()?;
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn foreground_arguments(shell_pid: u32) -> Vec<super::ProcessArguments> {
    let Some(foreground) = get_foreground_pid(shell_pid) else {
        return Vec::new();
    };
    let group = unsafe { libc::getpgid(foreground as i32) };
    if group < 0 {
        return Vec::new();
    }
    collect_pid_tree(foreground)
        .into_iter()
        .filter_map(|pid| {
            if unsafe { libc::getpgid(pid as i32) } != group {
                return None;
            }
            let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
            let mut length = 0usize;
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    std::ptr::null_mut(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
                || length > 256 * 1024
            {
                return None;
            }
            let mut bytes = vec![0u8; length];
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    bytes.as_mut_ptr().cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
            {
                return None;
            }
            bytes.truncate(length);
            let count = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
            if !(1..=4096).contains(&count) {
                return None;
            }
            let tail = bytes.get(4..)?;
            let executable_end = tail.iter().position(|byte| *byte == 0)?;
            let padding = tail.get(executable_end + 1..)?;
            let start = padding.iter().position(|byte| *byte != 0)?;
            let mut arguments = padding[start..].split(|byte| *byte == 0);
            let argv: Vec<String> = arguments
                .by_ref()
                .take(count as usize)
                .map(|word| std::str::from_utf8(word).map(str::to_owned))
                .collect::<Result<_, _>>()
                .ok()?;

            Some(super::ProcessArguments {
                argv,
                stdin_is_tty: None,
            })
        })
        .collect()
}

pub(super) fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    let output = Command::new("ps")
        .args(["-o", "ppid=,args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, fields.next().unwrap_or_default().to_string()))
}

pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "tpgid=", "-p", &shell_pid.to_string()])
        .output()
        .ok()?;

    if !output.status.success() {
        return Some(shell_pid);
    }

    let tpgid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;

    if tpgid <= 0 {
        return Some(shell_pid);
    }

    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

fn find_process_in_group(pgrp: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "pid=,pgid=", "-A"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(pid), Ok(proc_pgrp)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                if proc_pgrp == pgrp {
                    return Some(pid);
                }
            }
        }
    }

    None
}

/// Prevents user-idle system sleep by holding a `caffeinate` child. `-i`
/// inhibits system idle sleep only, so the display still sleeps normally.
pub(super) struct CaffeinateInhibitor {
    child: Option<std::process::Child>,
}

impl CaffeinateInhibitor {
    pub(super) fn new() -> Self {
        Self { child: None }
    }
}

impl super::SleepInhibit for CaffeinateInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        // `-w <daemon_pid>` releases the assertion even when the daemon dies without `Drop`.
        let child = match Command::new("caffeinate")
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::latch_sleep_inhibit_unavailable(
                    "caffeinate not found; OS sleep will not be inhibited on this host",
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn is_held_alive(&mut self) -> bool {
        super::sleep_inhibit_child_held_alive(
            &mut self.child,
            "caffeinate exited unexpectedly; OS sleep will not be inhibited on this host",
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn staging_inventory_uses_anchored_image_and_mount_identities() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let image = parent.path().join("volume.dmg");
        let root = parent.path().join("mount");
        let alias = parent.path().join("alias");
        std::fs::write(&image, b"image").unwrap();
        std::fs::create_dir(&root).unwrap();
        symlink(parent.path(), &alias).unwrap();
        let held = std::fs::File::open(&image).unwrap();
        let image_identity = metadata_identity(&held.metadata().unwrap());
        let mount_identity = metadata_identity(&std::fs::metadata(&root).unwrap());
        let snapshot = |paths: &[std::path::PathBuf], mount: &std::path::Path, device: &str| {
            let records = paths
                .iter()
                .map(|path| {
                    format!(
                        "<dict><key>image-path</key><string>{}</string>\
                        <key>system-entities</key><array><dict>\
                        <key>mount-point</key><string>{}</string>\
                        <key>dev-entry</key><string>{device}</string>\
                        </dict></array></dict>",
                        path.display(),
                        mount.display()
                    )
                })
                .collect::<String>();
            staging_plist(
                format!(
                    "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
                    <key>images</key><array>{records}</array></dict></plist>"
                )
                .as_bytes(),
            )
            .unwrap()
        };
        let verify = |inventory: &core_foundation::dictionary::CFDictionary, attached| {
            staging_verify_inventory(
                inventory,
                &image,
                image_identity,
                &root,
                mount_identity,
                "/dev/disk42",
                attached,
            )
        };
        let aliased_image = alias.join("volume.dmg");
        let aliased_mount = alias.join("mount");
        let inventory = snapshot(
            std::slice::from_ref(&aliased_image),
            &aliased_mount,
            "/dev/disk42",
        );
        assert!(verify(&inventory, true).is_ok());
        assert!(verify(&inventory, false).is_err());
        assert!(staging_verify_unattached_inventory(&inventory, image_identity).is_err());
        let images = [image.clone(), aliased_image.clone()];
        let duplicate = snapshot(&images, &root, "/dev/disk42");
        assert!(verify(&duplicate, true).is_err());
        let changed_device = snapshot(std::slice::from_ref(&image), &root, "/dev/disk43");
        assert!(verify(&changed_device, true).is_err());
        let other_mount = parent.path().join("other-mount");
        std::fs::create_dir(&other_mount).unwrap();
        let changed_mount = snapshot(std::slice::from_ref(&image), &other_mount, "/dev/disk42");
        assert!(verify(&changed_mount, true).is_err());
        let other_image = parent.path().join("other.dmg");
        std::fs::write(&other_image, b"unrelated image").unwrap();
        let recycled = snapshot(
            std::slice::from_ref(&other_image),
            &other_mount,
            "/dev/disk42",
        );
        assert!(verify(&recycled, false).is_ok());
        assert!(verify(&recycled, true)
            .unwrap_err()
            .is::<StagingInventoryMissing>());
        let incomplete = staging_plist(
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
                <key>images</key><array>\
                <dict><key>image-path</key><string>{}</string>\
                <key>system-entities</key><array><dict>\
                <key>mount-point</key><string>{}</string>\
                <key>dev-entry</key><string>/dev/disk42</string>\
                </dict></array></dict>\
                <dict><key>image-path</key><string>{}</string>\
                <key>system-entities</key><array><dict/></array></dict>\
                </array></dict></plist>",
                image.display(),
                root.display(),
                other_image.display(),
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(verify(&incomplete, true).is_ok());
        std::fs::remove_file(&other_image).unwrap();
        assert!(verify(&incomplete, true).is_ok());
        assert!(verify(&recycled, false).is_ok());
        assert!(staging_verify_unattached_inventory(&recycled, image_identity).is_ok());
        assert!(staging_verify_unattached_inventory(&incomplete, image_identity).is_err());
        std::fs::write(&other_image, b"unrelated image").unwrap();
        let without_entities = |path: &std::path::Path| {
            staging_plist(
                format!(
                    "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
                    <key>images</key><array><dict>\
                    <key>image-path</key><string>{}</string>\
                    </dict></array></dict></plist>",
                    path.display()
                )
                .as_bytes(),
            )
            .unwrap()
        };
        let sparse_bundle = parent.path().join("foreign.sparsebundle");
        std::fs::create_dir(&sparse_bundle).unwrap();
        let bundled_inventory = without_entities(&sparse_bundle);
        assert!(verify(&bundled_inventory, true)
            .unwrap_err()
            .is::<StagingInventoryMissing>());
        assert!(staging_verify_unattached_inventory(&bundled_inventory, image_identity).is_ok());
        let incomplete_foreign = without_entities(&other_image);
        assert!(verify(&incomplete_foreign, true)
            .unwrap_err()
            .is::<StagingInventoryMissing>());
        let incomplete_owned = without_entities(&image);
        assert!(!verify(&incomplete_owned, true)
            .unwrap_err()
            .is::<StagingInventoryMissing>());
        let empty = snapshot(&[], &root, "/dev/disk42");
        assert!(verify(&empty, false).is_ok());
        let error = verify(&empty, true).unwrap_err().to_string();
        assert!(error.contains("found=0"));
        assert!(error.contains(&image.display().to_string()));

        // Drive the real inventory validator with a virtual monotonic clock.
        // Publication, deadline and contradictions need no mounted image or sleep.
        let await_snapshots = |snapshots: &[&core_foundation::dictionary::CFDictionary],
                               timeout| {
            use std::cell::Cell;
            use std::time::Duration;
            let observations = Cell::new(0usize);
            let elapsed = Cell::new(Duration::ZERO);
            let result = staging_wait_for_inventory(
                || {
                    let index = observations.get();
                    observations.set(index + 1);
                    verify(snapshots[index.min(snapshots.len() - 1)], true)
                },
                || elapsed.get(),
                |duration| elapsed.set(elapsed.get() + duration),
                timeout,
            );
            (result, observations.get(), elapsed.get())
        };
        let budget = std::time::Duration::from_millis(50);
        let tick = std::time::Duration::from_millis(25);
        let (published, observations, elapsed) = await_snapshots(&[&empty, &inventory], budget);
        assert!(published.is_ok());
        assert_eq!(observations, 2);
        assert_eq!(elapsed, tick);

        let unrelated = snapshot(
            std::slice::from_ref(&other_image),
            &other_mount,
            "/dev/disk43",
        );
        let (missing, observations, elapsed) = await_snapshots(&[&empty, &unrelated], budget);
        let error = missing.unwrap_err().to_string();
        assert!(error.contains("before deadline"));
        assert!(error.contains("elapsed=50ms"));
        assert!(error.contains("observations=2"));
        assert!(error.contains("first_inventory="));
        assert!(error.contains("inventory=[]"));
        assert!(error.contains("last_inventory="));
        assert!(error.contains(&other_image.display().to_string()));
        assert_eq!(observations, 2);
        assert_eq!(elapsed, budget);

        let unknown_alias = snapshot(
            &[image.clone(), parent.path().join("unknown-image-alias.dmg")],
            &root,
            "/dev/disk42",
        );
        let conflicting_mount = snapshot(std::slice::from_ref(&other_image), &root, "/dev/disk43");
        for (contradiction, message) in [
            (&duplicate, "found=2"),
            (&changed_device, "changed device/mount"),
            (&changed_mount, "unexpected mountpoint"),
            (&conflicting_mount, "another inventory image claims"),
            (&unknown_alias, "another inventory image claims"),
        ] {
            let (rejected, observations, elapsed) =
                await_snapshots(&[&empty, contradiction, &inventory], budget);
            let error = rejected.unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert!(error.downcast_ref::<StagingInventoryMissing>().is_none());
            assert_eq!(observations, 2);
            assert_eq!(elapsed, tick, "contradictions must not wait or recover");
        }
        // A mount path that resolves to another directory must not inherit the
        // identity captured when attachment completed.
        std::fs::rename(&root, parent.path().join("retained-mount")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(verify(&inventory, true).is_err());
        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(parent.path().join("retained-mount"), &root).unwrap();

        // The old descriptor stays live, so replacement cannot reuse its inode.
        std::fs::rename(&image, parent.path().join("retained.dmg")).unwrap();
        std::fs::write(&image, b"replacement").unwrap();
        assert!(verify(&inventory, true).is_err());
        std::fs::remove_file(&image).unwrap();
        symlink(parent.path().join("retained.dmg"), &image).unwrap();
        assert!(verify(&inventory, true).is_err());
        std::fs::remove_file(&image).unwrap();
        std::fs::create_dir(&image).unwrap();
        let owned_directory = snapshot(std::slice::from_ref(&image), &root, "/dev/disk42");
        assert!(!verify(&owned_directory, true)
            .unwrap_err()
            .is::<StagingInventoryMissing>());
    }

    use super::*;

    #[test]
    fn codex_managed_preferences_refuse_either_key() {
        for (config, requirements, expected) in [
            (false, false, false),
            (true, false, true),
            (false, true, true),
        ] {
            let present = codex_managed_preferences_present_with(|key| match key {
                "config_toml_base64" => config,
                "requirements_toml_base64" => requirements,
                _ => unreachable!("{key}"),
            });
            assert_eq!(present, expected, "{config} {requirements}");
        }
        let unused = format!("dev.aoe.test.{}", uuid::Uuid::new_v4());
        assert!(!preference_value_present("config_toml_base64", &unused));
    }

    #[test]
    fn foreign_runner_probe_requires_an_empty_original_group_and_keeps_native_tokens_strict() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let available = Command::new("sudo")
            .args(["-n", "-u", "nobody", "true"])
            .status();
        if !available.is_ok_and(|status| status.success()) {
            eprintln!("skipping foreign-runner probe: passwordless sudo unavailable");
            return;
        }
        struct Foreign(std::process::Child);
        impl Drop for Foreign {
            fn drop(&mut self) {
                self.0.stdin.take();
                let _ = self.0.wait();
            }
        }
        for own_group in [false, true] {
            let script = format!(
                "import os,sys\npid=os.fork()\nif pid == 0:\n    {}\n    print(os.getpid(),flush=True)\n    sys.stdin.read()\n    os._exit(0)\nelse:\n    _,status=os.waitpid(pid,0)\n    sys.exit(os.waitstatus_to_exitcode(status))",
                if own_group { "os.setsid()" } else { "pass" }
            );
            let mut child = Foreign(
                Command::new("sudo")
                    .args(["-n", "-u", "nobody", "python3", "-c", &script])
                    .current_dir("/tmp")
                    .process_group(0)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut line = String::new();
            std::io::BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let pid = line.trim().parse::<u32>().unwrap();
            assert!(
                process_start_id(pid).is_err(),
                "generic native ownership must remain unproven"
            );
            assert_eq!(foreign_runner_group_absent(pid).unwrap(), !own_group);
            if own_group {
                assert!(super::super::runner_birth_identity(pid).is_err());
            } else {
                assert!(super::super::runner_birth_identity(pid).unwrap().is_none());
                let guard = super::super::RunnerProcessGuard::capture(pid, None).unwrap();
                let mut words = [0; 8];
                words[5] = pid;
                {
                    let mut members = guard.members.lock();
                    let index = members.processes.len();
                    members.active.insert(pid, index);
                    members.processes.push((
                        super::super::PrimeProcessIdentity {
                            pid,
                            start_id: audit_start_id(&AuditToken { values: words }),
                        },
                        ProcessHandle {
                            initial_token: AuditToken { values: words },
                            owned_birth: None,
                        },
                    ));
                }
                let auxiliary = Command::new("cat")
                    .process_group(0)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                struct Auxiliary(std::process::Child);
                impl Drop for Auxiliary {
                    fn drop(&mut self) {
                        let _ = self.0.kill();
                        let _ = self.0.wait();
                    }
                }
                let auxiliary_identity = super::super::PrimeProcessIdentity {
                    pid: auxiliary.id(),
                    start_id: process_start_id(auxiliary.id()).unwrap().unwrap(),
                };
                let auxiliary_handle = ProcessHandle::capture(&auxiliary_identity)
                    .unwrap()
                    .unwrap();
                let mut auxiliary = Auxiliary(auxiliary);
                {
                    let mut members = guard.members.lock();
                    let index = members.processes.len();
                    members.active.insert(auxiliary_identity.pid, index);
                    members
                        .processes
                        .push((auxiliary_identity, auxiliary_handle));
                }
                assert!(
                    guard.executing().unwrap(),
                    "a known detached member must remain owned"
                );
                guard.signal(libc::SIGTERM).unwrap();
                assert!(!auxiliary.0.wait().unwrap().success());
                assert!(!guard.executing().unwrap());
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "the foreign execution was signalled"
            );
            drop(child.0.stdin.take());
            assert!(child.0.wait().unwrap().success());
        }
    }

    #[test]
    fn prime_quiescence_accepts_observed_owned_zombies() {
        use std::io::{BufRead as _, Write as _};
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let marker = format!("TMPDIR={}", root.path().display());
        let socket = root.path().join("daemon.sock");
        let mut child = OwnedChild(
            Command::new("sh")
                .args(["-c", "printf 'ready\n'; IFS= read -r line"])
                .arg("sh")
                .arg(&socket)
                .env("TMPDIR", root.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let owned = prime_processes(&marker, &socket, &[])
            .unwrap()
            .into_iter()
            .find(|identity| identity.pid == child.0.id())
            .unwrap();
        child.0.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let mut probe = Command::new("ps");
            probe.args(["-p", &child.0.id().to_string(), "-o", "stat="]);
            let state = crate::session::capture::run_with_timeout_limit(
                probe,
                std::time::Duration::from_secs(1),
                "owned zombie observation",
                1024,
            )
            .unwrap();
            if std::str::from_utf8(&state).unwrap().trim().starts_with('Z') {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned child did not become a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let remaining = prime_processes(&marker, &socket, std::slice::from_ref(&owned)).unwrap();
        assert!(!remaining.contains(&owned));
        assert!(child.0.wait().unwrap().success());
    }

    #[test]
    fn attested_runner_root_is_retained_once_across_refreshes() {
        use std::os::unix::process::CommandExt as _;
        struct Leader(std::process::Child);
        impl Drop for Leader {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let leader = Leader(
            Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let pid = leader.0.id();
        let birth = super::super::runner_birth_identity(pid)
            .unwrap()
            .expect("live leader has a birth identity");
        let root = super::super::PrimeProcessIdentity {
            pid,
            start_id: birth.start_id.clone(),
        };
        assert!(
            runner_group_identities(pid).unwrap().contains(&root),
            "enumeration must include the live leader"
        );

        let guard =
            super::super::RunnerProcessGuard::capture(pid, Some(birth.fingerprint())).unwrap();
        let roots = |guard: &super::super::RunnerProcessGuard| {
            guard
                .members
                .lock()
                .processes
                .iter()
                .filter(|(identity, _)| identity.pid == pid)
                .count()
        };
        assert_eq!(roots(&guard), 1, "root duplicated by capture");
        for _ in 0..3 {
            guard.refresh(&mut guard.members.lock()).unwrap();
            assert_eq!(roots(&guard), 1, "root duplicated by refresh");
        }
    }

    #[test]
    fn original_child_custody_distinguishes_physical_birth_from_execution() {
        use crate::process::{CapturedProcessTree, OwnedProcessGroup, OwnedProcessTree};
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        struct Original(Option<OwnedProcessGroup>, Option<ProcessHandle>);
        impl Drop for Original {
            fn drop(&mut self) {
                if let Some(handle) = self.1.take() {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let _ = handle.signal_owned(libc::SIGKILL, deadline);
                    while handle.physically_exited().is_ok_and(|exited| !exited)
                        && Instant::now() < deadline
                    {
                        std::thread::yield_now();
                    }
                }
                if let Some(mut group) = self.0.take() {
                    let _ = group.settle(Duration::ZERO);
                    let _ = group.reap();
                }
            }
        }
        fn published_pid(path: &std::path::Path) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(pid) = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                {
                    return pid;
                }
                assert!(Instant::now() < deadline, "child did not publish readiness");
                std::thread::yield_now();
            }
        }

        let mut completed = Original(
            Some(OwnedProcessGroup::spawn(&mut Command::new("true")).unwrap()),
            None,
        );
        let group = completed.0.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !group.exited().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!runner_group_executing(group.id()).unwrap());
        completed
            .0
            .as_mut()
            .unwrap()
            .settle(Duration::ZERO)
            .unwrap();
        assert!(completed.0.take().unwrap().reap().unwrap().success());

        for (descendant_exec, escape) in [(false, false), (true, false), (true, true)] {
            let effects = tempfile::tempdir().unwrap();
            std::fs::write(
                effects.path().join("first.sh"),
                "printf '%s' \"$$\" > ready\nIFS= read -r release\nexec sh second.sh\n",
            )
            .unwrap();
            std::fs::write(
                effects.path().join("second.sh"),
                if escape {
                    "exec python3 -c 'import os,sys;os.setsid();open(\"advanced\",\"w\").write(str(os.getpid()));sys.stdin.readline()'\n"
                } else {
                    "printf '%s' \"$$\" > advanced\nIFS= read -r release\n"
                },
            )
            .unwrap();
            let mut command = Command::new("sh");
            if descendant_exec {
                command.args(["-c", "sh first.sh <&0 & wait \"$!\""]);
            } else {
                command.arg("first.sh");
            }
            command
                .current_dir(effects.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut original =
                Original(Some(OwnedProcessGroup::spawn(&mut command).unwrap()), None);
            let mut input = original.0.as_mut().unwrap().child.stdin.take().unwrap();
            let group = original.0.as_ref().unwrap();
            let pid = published_pid(&effects.path().join("ready"));
            let before = audit_token(pid).unwrap().unwrap();
            let stale = CapturedProcessTree::capture(pid).unwrap();
            if descendant_exec {
                original.1 = ProcessHandle::capture_owned(
                    &super::super::PrimeProcessIdentity {
                        pid,
                        start_id: String::new(),
                    },
                    group,
                )
                .unwrap();
            }
            let mut owned = OwnedProcessTree::capture(group).unwrap();
            input.write_all(b"advance\n").unwrap();
            assert_eq!(published_pid(&effects.path().join("advanced")), pid);
            let after = audit_token(pid).unwrap().unwrap();
            assert_ne!(before.values[7], after.values[7]);
            assert!(!group.exited().unwrap());

            let recovered = CapturedProcessTree::resume(stale.receipt().clone(), pid).unwrap();
            recovered.settle(Duration::ZERO).unwrap();
            assert!(audit_token(pid).unwrap().is_some());
            owned.refresh().unwrap();
            if descendant_exec {
                assert!(!owned
                    .members
                    .get(&pid)
                    .unwrap()
                    .physically_exited()
                    .unwrap());
                let handle = owned.members.get(&pid).unwrap();
                let actual_birth = handle.owned_birth.unwrap();
                let stale_identity = super::super::PrimeProcessIdentity {
                    pid,
                    start_id: audit_start_id(&before),
                };
                assert!(ProcessHandle::capture(&stale_identity).unwrap().is_none());
                let freshly_owned = ProcessHandle::capture_owned(&stale_identity, group)
                    .unwrap()
                    .unwrap();
                assert!(!freshly_owned.physically_exited().unwrap());
                let replacement = ProcessHandle {
                    initial_token: handle.initial_token,
                    owned_birth: Some(actual_birth.wrapping_add(1)),
                };
                replacement
                    .signal_owned(libc::SIGKILL, Instant::now() + Duration::from_secs(1))
                    .unwrap();
                assert!(
                    audit_token(pid).unwrap().is_some(),
                    "different birth was signalled"
                );
                owned.settle(Duration::ZERO).unwrap();
                assert!(audit_token(pid).unwrap().is_none());
            } else {
                assert_eq!(pid, group.id());
                owned.settle(Duration::ZERO).unwrap();
                assert!(!group.exited().unwrap());
            }
            drop(owned);
            original.0.as_mut().unwrap().settle(Duration::ZERO).unwrap();
            assert!(!runner_group_executing(original.0.as_ref().unwrap().id()).unwrap());
            original.1.take();
            assert!(!original.0.take().unwrap().reap().unwrap().success());
        }
    }

    #[test]
    #[serial_test::serial]
    fn captured_process_custody_survives_an_unavailable_sysctl_command() {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;

        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = OwnedChild(
            Command::new("sh")
                .args(["-c", "printf 'ready\\n'; IFS= read -r line"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let birth = super::super::process_birth_identity(child.0.id())
            .unwrap()
            .unwrap();
        let tools = tempfile::tempdir().unwrap();
        let sysctl = tools.path().join("sysctl");
        std::fs::write(&sysctl, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&sysctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _path = crate::session::test_support::path_prepended(tools.path());
        let captured = super::super::CapturedProcess::capture(child.0.id(), &birth)
            .unwrap()
            .expect("an unavailable utility must not discard custody of a live process");
        assert!(!captured.exited().unwrap());
        child.0.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        assert!(child.0.wait().unwrap().success());
        assert!(captured.exited().unwrap());
    }
}
