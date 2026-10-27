//! Linux-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = false;
use anyhow::Context as _;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

pub(super) use super::unix::{
    close_frozen_payload, configure_process_group, exec_command, kill_process_group,
    restore_environment_value, terminate_process_group, try_wait_status_hook,
};

struct LegacyRuntimeDirectories {
    run: std::os::fd::OwnedFd,
    user: Option<std::os::fd::OwnedFd>,
    runtime: Option<std::os::fd::OwnedFd>,
}

impl LegacyRuntimeDirectories {
    fn open(runtime: &Path, uid: u32, anchor_uid: u32) -> anyhow::Result<Self> {
        use nix::fcntl::open;
        use nix::sys::stat::Mode;
        let run_path = runtime
            .parent()
            .and_then(Path::parent)
            .context("Invalid runtime path")?;
        let uid_name = runtime
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid runtime user directory")?;
        let run = open(run_path, Self::flags(), Mode::empty())?;
        Self::validate(&run, anchor_uid)?;
        let user = Self::child(&run, "user", anchor_uid)?;
        let directory = match &user {
            Some(user) => Self::child(user, uid_name, uid)?,
            None => None,
        };
        Ok(Self {
            run,
            user,
            runtime: directory,
        })
    }

    fn flags() -> nix::fcntl::OFlag {
        use nix::fcntl::OFlag;
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY
    }

    fn validate(directory: &std::os::fd::OwnedFd, uid: u32) -> anyhow::Result<()> {
        let metadata = nix::sys::stat::fstat(directory)?;
        anyhow::ensure!(
            metadata.st_uid == uid && metadata.st_mode & 0o022 == 0,
            "User manager runtime directory is unsafe"
        );
        Ok(())
    }

    fn child(
        parent: &std::os::fd::OwnedFd,
        name: &str,
        uid: u32,
    ) -> anyhow::Result<Option<std::os::fd::OwnedFd>> {
        use nix::errno::Errno;
        use nix::fcntl::openat;
        use nix::sys::stat::Mode;
        match openat(parent, name, Self::flags(), Mode::empty()) {
            Ok(directory) => {
                Self::validate(&directory, uid)?;
                Ok(Some(directory))
            }
            Err(Errno::ENOENT) => Ok(None),
            Err(error) => Err(error).context("User manager runtime directory is unavailable"),
        }
    }

    fn check_current(&self, runtime: &Path, uid: u32, anchor_uid: u32) -> anyhow::Result<()> {
        let current = Self::open(runtime, uid, anchor_uid)?;
        for (old, new) in [
            (Some(&self.run), Some(&current.run)),
            (self.user.as_ref(), current.user.as_ref()),
            (self.runtime.as_ref(), current.runtime.as_ref()),
        ] {
            let same = match (old, new) {
                (Some(old), Some(new)) => {
                    let old = nix::sys::stat::fstat(old)?;
                    let new = nix::sys::stat::fstat(new)?;
                    old.st_dev == new.st_dev && old.st_ino == new.st_ino
                }
                (None, None) => true,
                _ => false,
            };
            anyhow::ensure!(
                same,
                "User manager runtime boundary changed during scope proof"
            );
        }
        Ok(())
    }
}

struct LegacyUserManager {
    stream: std::os::unix::net::UnixStream,
    pid: u32,
    uid: u32,
    birth: super::ProcessBirthIdentity,
    namespace: std::path::PathBuf,
    process_cgroup: String,
    root: String,
    runtime: std::path::PathBuf,
    runtime_directories: LegacyRuntimeDirectories,
    anchor_uid: u32,
    systemd_directory: std::os::fd::OwnedFd,
    private_identity: (u64, u64),
    mount_root_record: String,
}

impl LegacyUserManager {
    fn command(&self) -> anyhow::Result<Command> {
        let mut command = Command::new(which::which("systemctl")?);
        command
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", self.runtime.display()),
            )
            .env_remove("SYSTEMD_BUS_ADDRESS")
            .env("LC_ALL", "C")
            .env("SYSTEMD_COLORS", "0");
        Ok(command)
    }

    fn manager_root(&self) -> anyhow::Result<String> {
        let mut command = self.command()?;
        command.args(["--user", "show", "--property=ControlGroup"]);
        let output = super::run_with_timeout(&mut command, std::time::Duration::from_secs(3))?
            .context("User manager cgroup query timed out")?;
        anyhow::ensure!(
            output.status.success(),
            "User manager cgroup query rejected"
        );
        let text = std::str::from_utf8(&output.stdout)?;
        let mut lines = text.lines();
        let root = lines
            .next()
            .and_then(|line| line.strip_prefix("ControlGroup="))
            .context("User manager cgroup root is unavailable")?;
        anyhow::ensure!(lines.next().is_none(), "Malformed user manager cgroup root");
        validate_legacy_manager_root(root)?;
        Ok(root.to_owned())
    }

    fn capture(prefix: &str) -> anyhow::Result<Option<Self>> {
        use std::os::fd::AsRawFd as _;
        let uid = nix::unistd::geteuid().as_raw();
        let runtime = std::path::PathBuf::from(format!("/run/user/{uid}"));
        let anchor_uid = 0;
        #[cfg(test)]
        let (runtime, anchor_uid) = super::systemd::legacy_runtime_fixture::root()
            .map(|root| (root.join("user").join(uid.to_string()), uid))
            .unwrap_or((runtime, anchor_uid));
        let runtime_directories = LegacyRuntimeDirectories::open(&runtime, uid, anchor_uid)?;
        let Some(directory) = &runtime_directories.runtime else {
            runtime_directories.check_current(&runtime, uid, anchor_uid)?;
            return Ok(None);
        };
        let Some(systemd_directory) = LegacyRuntimeDirectories::child(directory, "systemd", uid)?
        else {
            anyhow::ensure!(
                kernel_scope_inventory_clear(prefix)?,
                "Legacy scope cgroup is still populated; original custody retained"
            );
            runtime_directories.check_current(&runtime, uid, anchor_uid)?;
            anyhow::ensure!(
                LegacyRuntimeDirectories::child(directory, "systemd", uid)?.is_none(),
                "User manager appeared during scope absence proof"
            );
            return Ok(None);
        };
        let private = runtime_io::stat_entry(systemd_directory.as_raw_fd(), "private")?;
        anyhow::ensure!(
            private.st_mode & runtime_io::KIND_MASK == runtime_io::SOCKET && private.st_uid == uid,
            "User manager private socket is unsafe"
        );
        let private_identity = (private.st_dev, private.st_ino);
        let private = runtime_io::anchored_path(systemd_directory.as_raw_fd(), "private")?;
        let stream = super::worker::connect_with_timeout(&private)
            .context("Authenticated user manager is unavailable")?;
        let peer = runtime_io::peer_credentials(stream.as_raw_fd())?;
        anyhow::ensure!(
            peer.uid == uid && peer.pid != 0,
            "User manager peer does not match the effective user"
        );
        let birth = super::process_birth_identity(peer.pid)?
            .context("User manager process birth is unavailable")?;
        let namespace = fs::read_link("/proc/self/ns/cgroup")?;
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        let mount_root_record = validate_legacy_cgroup_mount(&mountinfo)?.to_owned();
        let process_cgroup = fs::read_to_string(format!("/proc/{}/cgroup", peer.pid))?;
        let mut manager = Self {
            stream,
            pid: peer.pid,
            uid,
            birth,
            namespace,
            process_cgroup,
            root: String::new(),
            runtime,
            runtime_directories,
            anchor_uid,
            systemd_directory,
            private_identity,
            mount_root_record,
        };
        manager.root = manager.manager_root()?;
        validate_legacy_manager_membership(&manager.process_cgroup, &manager.root)?;
        let directory = open_cgroup_directory(&manager.inventory_root())?;
        require_cgroup2(&directory)?;
        manager.check_current()?;
        Ok(Some(manager))
    }

    /// Loaded scopes named `<prefix>*.scope`, with their active state.
    fn list_scope_units(&self, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
        use std::collections::HashSet;
        use std::time::Duration;
        let mut command = self.command()?;
        command.args([
            "--user",
            "list-units",
            "--all",
            "--plain",
            "--no-legend",
            "--no-pager",
            "--full",
            &format!("{prefix}*.scope"),
        ]);
        command.env("LC_ALL", "C").env("SYSTEMD_COLORS", "0");
        let output = super::run_with_timeout(&mut command, Duration::from_secs(3))?
            .context("Legacy scope inventory timed out")?;
        anyhow::ensure!(
            output.status.success(),
            "Legacy scope inventory rejected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut listed = HashSet::new();
        let mut units = Vec::new();
        for line in std::str::from_utf8(&output.stdout)?.lines() {
            let mut fields = line.split_whitespace();
            let unit = fields.next().context("Missing legacy scope unit")?;
            fields.next().context("Missing legacy scope load state")?;
            let active = fields.next().context("Missing legacy scope active state")?;
            fields.next().context("Missing legacy scope substate")?;
            anyhow::ensure!(
                unit.starts_with(prefix) && unit.ends_with(".scope") && listed.insert(unit),
                "Invalid or duplicate legacy scope inventory unit"
            );
            units.push((unit.to_owned(), active.to_owned()));
        }
        Ok(units)
    }

    fn inventory_root(&self) -> std::path::PathBuf {
        std::path::PathBuf::from("/sys/fs/cgroup").join(self.root.trim_start_matches('/'))
    }

    fn check_current(&self) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd as _;
        self.runtime_directories
            .check_current(&self.runtime, self.uid, self.anchor_uid)?;
        let directory = self
            .runtime_directories
            .runtime
            .as_ref()
            .context("User manager runtime directory disappeared")?;
        let systemd = LegacyRuntimeDirectories::child(directory, "systemd", self.uid)?
            .context("User manager private socket directory disappeared")?;
        let old = nix::sys::stat::fstat(&self.systemd_directory)?;
        let new = nix::sys::stat::fstat(&systemd)?;
        let private = runtime_io::stat_entry(systemd.as_raw_fd(), "private")?;
        anyhow::ensure!(
            old.st_dev == new.st_dev
                && old.st_ino == new.st_ino
                && (private.st_dev, private.st_ino) == self.private_identity
                && private.st_mode & runtime_io::KIND_MASK == runtime_io::SOCKET
                && private.st_uid == self.uid,
            "User manager private socket changed during scope proof"
        );
        let peer = runtime_io::peer_credentials(self.stream.as_raw_fd())?;
        anyhow::ensure!(
            peer.uid == self.uid
                && peer.pid == self.pid
                && super::process_birth_identity(self.pid)?.as_ref() == Some(&self.birth),
            "User manager incarnation changed during scope proof"
        );
        anyhow::ensure!(
            fs::read_link("/proc/self/ns/cgroup")? == self.namespace
                && fs::read_to_string(format!("/proc/{}/cgroup", self.pid))? == self.process_cgroup
                && validate_legacy_cgroup_mount(&fs::read_to_string("/proc/self/mountinfo")?)?
                    == self.mount_root_record
                && self.manager_root()? == self.root,
            "User manager inventory boundary changed during scope proof"
        );
        let members = fs::read_to_string(self.inventory_root().join("init.scope/cgroup.procs"))?;
        validate_legacy_manager_pid(&members, self.pid)?;
        anyhow::ensure!(
            super::process_birth_identity(self.pid)?.as_ref() == Some(&self.birth),
            "User manager incarnation changed during kernel membership proof"
        );
        Ok(())
    }
}

fn validate_legacy_manager_root(root: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        root.starts_with('/')
            && root != "/"
            && !root.ends_with('/')
            && root
                .split('/')
                .skip(1)
                .all(|part| !part.is_empty() && !matches!(part, "." | "..")),
        "User manager cgroup root is not a complete non-root absolute path"
    );
    Ok(())
}

fn validate_legacy_manager_membership(contents: &str, root: &str) -> anyhow::Result<()> {
    // Procfs uses the reader's root; systemd reports its manager's root.
    let mut unified = contents.lines().filter_map(|line| line.strip_prefix("0::"));
    anyhow::ensure!(
        unified
            .next()
            .and_then(|path| path.strip_suffix("/init.scope"))
            == Some(root)
            && unified.next().is_none(),
        "User manager cgroup does not match the authenticated peer"
    );
    Ok(())
}

fn validate_legacy_cgroup_mount(contents: &str) -> anyhow::Result<&str> {
    let mut found = None;
    for line in contents.lines() {
        let (mount, filesystem) = line
            .split_once(" - ")
            .context("Malformed mount inventory")?;
        let mut fields = mount.split_whitespace();
        let root = fields.nth(3).context("Missing mount root")?;
        let point = fields.next().context("Missing mount point")?;
        anyhow::ensure!(
            !point.starts_with("/sys/fs/cgroup/"),
            "A descendant mount hides part of the cgroup inventory"
        );
        if point == "/sys/fs/cgroup" {
            anyhow::ensure!(
                found.is_none()
                    && root == "/"
                    && filesystem.split_whitespace().next() == Some("cgroup2"),
                "A complete unified cgroup hierarchy is unavailable"
            );
            found = Some(line);
        }
    }
    found.context("Unified cgroup mount is unavailable")
}

fn validate_legacy_manager_pid(contents: &str, expected: u32) -> anyhow::Result<()> {
    let mut found = false;
    for line in contents.lines() {
        let pid: u32 = line
            .parse()
            .context("Malformed manager cgroup membership")?;
        anyhow::ensure!(pid != 0, "Invalid manager cgroup PID");
        found |= pid == expected;
    }
    anyhow::ensure!(
        found,
        "Authenticated manager is absent from the visible cgroup"
    );
    Ok(())
}

#[cfg(test)]
mod legacy_inventory_boundary_tests {
    use super::*;

    type BeforeOpen = Box<dyn Fn(&std::ffi::OsStr)>;

    thread_local! {
        static BEFORE_OPEN: std::cell::RefCell<Option<BeforeOpen>> = const {
            std::cell::RefCell::new(None)
        };
    }

    struct RestoreBeforeOpen(Option<BeforeOpen>);

    impl Drop for RestoreBeforeOpen {
        fn drop(&mut self) {
            BEFORE_OPEN.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }

    pub(super) fn before_open(name: &std::ffi::OsStr) {
        BEFORE_OPEN.with(|slot| {
            if let Some(before_open) = slot.borrow().as_ref() {
                before_open(name);
            }
        });
    }

    #[test]
    fn vanished_foreign_cgroup_does_not_hide_surviving_scopes() {
        let prefix = "aoe-session-vanishing-fixture-";
        for population in [None, Some("populated 1\n"), Some("populated unknown\n")] {
            let root = tempfile::tempdir().unwrap();
            let vanished = root.path().join("vanishing.service");
            fs::create_dir(&vanished).unwrap();
            if let Some(population) = population {
                let scope = root.path().join(format!("{prefix}1700000000000.scope"));
                fs::create_dir(&scope).unwrap();
                fs::write(scope.join("cgroup.events"), population).unwrap();
            }
            let observed = std::rc::Rc::new(std::cell::Cell::new(0));
            let count = observed.clone();
            let _restore = RestoreBeforeOpen(BEFORE_OPEN.with(|slot| {
                slot.replace(Some(Box::new(move |name| {
                    if name == "vanishing.service" {
                        count.set(count.get() + 1);
                        fs::remove_dir(&vanished).unwrap();
                    }
                })))
            }));
            let result = legacy_scope_cgroups_clear(
                open_cgroup_directory(root.path()).unwrap(),
                prefix,
                &std::collections::HashSet::new(),
            );
            match population {
                None => assert!(result.unwrap()),
                Some("populated 1\n") => assert!(!result.unwrap()),
                _ => assert!(result.is_err()),
            }
            // The clear case must observe the vanished entry before proving absence.
            if population.is_none() {
                assert_eq!(observed.get(), 1);
            }
        }
    }

    #[test]
    fn replaced_cgroup_directory_does_not_prove_absence() {
        use std::os::unix::fs::symlink;
        for link in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            let target_path = target.path().to_owned();
            let replaced = root.path().join("replaced.service");
            fs::create_dir(&replaced).unwrap();
            let observed = std::rc::Rc::new(std::cell::Cell::new(false));
            let count = observed.clone();
            let _restore = RestoreBeforeOpen(BEFORE_OPEN.with(|slot| {
                slot.replace(Some(Box::new(move |name| {
                    assert_eq!(name, "replaced.service");
                    fs::remove_dir(&replaced).unwrap();
                    if link {
                        symlink(&target_path, &replaced).unwrap();
                    } else {
                        fs::write(&replaced, "").unwrap();
                    }
                    count.set(true);
                })))
            }));
            assert!(legacy_scope_cgroups_clear(
                open_cgroup_directory(root.path()).unwrap(),
                "aoe-session-replacement-fixture-",
                &std::collections::HashSet::new(),
            )
            .is_err());
            assert!(observed.get());
        }
    }

    #[test]
    fn native_kernel_inventory_rejects_a_fixture_filesystem() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("cgroup.controllers"), "").unwrap();
        let prefix = "aoe-session-native-boundary-fixture-";
        assert!(kernel_scope_inventory_clear_at(root.path(), prefix, false).unwrap());
        assert!(require_cgroup2(&open_cgroup_directory(root.path()).unwrap()).is_err());
        assert!(kernel_scope_inventory_clear_at(root.path(), prefix, true).is_err());
    }

    #[test]
    fn mount_boundary_ignores_foreign_changes_but_rejects_cgroup_changes() {
        let root = "31 20 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup rw";
        let before = format!("{root}\n32 20 0:28 / /media/old rw - tmpfs tmpfs rw\n");
        let after = format!("45 20 0:35 / /media/new ro - tmpfs tmpfs ro\n{root}\n");
        assert_eq!(
            validate_legacy_cgroup_mount(&before).unwrap(),
            validate_legacy_cgroup_mount(&after).unwrap()
        );
        for changed in [
            "33 20 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup rw",
            "31 21 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup rw",
            "31 20 0:29 / /sys/fs/cgroup rw - cgroup2 cgroup rw",
            "31 20 0:27 / /sys/fs/cgroup ro - cgroup2 cgroup rw",
            "31 20 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup ro",
        ] {
            assert_ne!(
                validate_legacy_cgroup_mount(root).unwrap(),
                validate_legacy_cgroup_mount(changed).unwrap()
            );
        }
        for invalid in [
            format!("{root}\n{root}\n"),
            format!("{root}\n40 31 0:30 / /sys/fs/cgroup/hidden rw - tmpfs tmpfs rw\n"),
            root.replace(" / /sys", " /subtree /sys"),
            root.replace(" - cgroup2 ", " - tmpfs "),
        ] {
            assert!(validate_legacy_cgroup_mount(&invalid).is_err());
        }
    }

    #[test]
    fn foreign_non_utf8_cgroups_do_not_hide_matching_descendants() {
        use std::os::unix::ffi::OsStrExt as _;
        let prefix = "aoe-session-byte-fixture-";
        for population in [
            None,
            Some("populated 0\n"),
            Some("populated 1\n"),
            Some("populated unknown\n"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let foreign = root
                .path()
                .join(std::ffi::OsStr::from_bytes(b"foreign-\xff.service"));
            fs::create_dir(&foreign).unwrap();
            if let Some(population) = population {
                let scope = foreign.join(format!("{prefix}1700000000000.scope"));
                fs::create_dir(&scope).unwrap();
                fs::write(scope.join("cgroup.events"), population).unwrap();
            }
            let result = legacy_scope_cgroups_clear(
                open_cgroup_directory(root.path()).unwrap(),
                prefix,
                &std::collections::HashSet::new(),
            );
            match population {
                None | Some("populated 0\n") => assert!(result.unwrap()),
                Some("populated 1\n") => assert!(!result.unwrap()),
                _ => assert!(result.is_err()),
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut matching = prefix.as_bytes().to_vec();
        matching.extend_from_slice(b"\xff.scope");
        let scope = root.path().join(std::ffi::OsStr::from_bytes(&matching));
        fs::create_dir(&scope).unwrap();
        fs::write(scope.join("cgroup.events"), "populated 0\n").unwrap();
        assert!(legacy_scope_cgroups_clear(
            open_cgroup_directory(root.path()).unwrap(),
            prefix,
            &std::collections::HashSet::new()
        )
        .is_err());
    }

    #[test]
    fn existing_managerless_runtime_requires_complete_empty_kernel_inventory() {
        use std::os::unix::fs::symlink;
        let prefix = "aoe-session-managerless-fixture-";
        for case in [
            "empty",
            "empty-scope",
            "populated",
            "nested",
            "known-populated",
            "missing-events",
            "malformed-events",
            "linked-events",
            "linked-directory",
            "partial-events",
            "partial-type",
            "missing-controllers",
            "linked-controllers",
        ] {
            let fixture = tempfile::tempdir().unwrap();
            let runtime = fixture
                .path()
                .join("user")
                .join(nix::unistd::geteuid().as_raw().to_string());
            fs::create_dir_all(&runtime).unwrap();
            let root = fixture.path().join("cgroups");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("cgroup.controllers"), "").unwrap();
            let unit = format!("{prefix}1700000000000.scope");
            if matches!(case, "partial-events" | "partial-type") {
                fs::write(
                    root.join(if case == "partial-events" {
                        "cgroup.events"
                    } else {
                        "cgroup.type"
                    }),
                    "",
                )
                .unwrap();
            } else if case == "missing-controllers" {
                fs::remove_file(root.join("cgroup.controllers")).unwrap();
            } else if case == "linked-controllers" {
                fs::remove_file(root.join("cgroup.controllers")).unwrap();
                symlink("missing", root.join("cgroup.controllers")).unwrap();
            } else if case == "linked-directory" {
                symlink(&runtime, root.join("hidden")).unwrap();
            } else if case != "empty" {
                let parent = if case == "nested" {
                    root.join("other.service")
                } else {
                    root.clone()
                };
                let scope = parent.join(&unit);
                fs::create_dir_all(&scope).unwrap();
                match case {
                    "missing-events" => {}
                    "linked-events" => {
                        fs::write(runtime.join("events"), "populated 0\n").unwrap();
                        symlink(runtime.join("events"), scope.join("cgroup.events")).unwrap();
                    }
                    _ => fs::write(
                        scope.join("cgroup.events"),
                        match case {
                            "empty-scope" => "populated 0\n",
                            "malformed-events" => "populated 0\npopulated 1\n",
                            _ => "populated 1\n",
                        },
                    )
                    .unwrap(),
                }
            }
            let _guard = super::super::systemd::legacy_runtime_fixture::install(fixture.path());
            let names = legacy_scope_unit_names(prefix);
            let known = [crate::process::worker_registry::ScopeOwnership::Owned { unit }];
            let clear = legacy_scope_inventory_clear(prefix, &known);
            if matches!(case, "empty" | "empty-scope") {
                assert!(names.unwrap().is_empty(), "{case}");
                assert!(clear.unwrap(), "{case}");
            } else {
                assert!(names.is_err(), "{case}: cannot discard original debt");
                assert!(clear.is_err(), "{case}: cannot discard captured units");
            }
        }
    }

    #[test]
    fn canonical_runtime_absence_requires_safe_complete_directory_evidence() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let uid = nix::unistd::geteuid().as_raw();
        for case in [
            "missing-user",
            "missing-runtime",
            "empty-runtime",
            "private-file",
            "private-link",
            "systemd-link",
            "runtime-link",
            "user-link",
            "user-file",
            "unsafe-run",
            "unsafe-runtime",
            "wrong-owner",
            "unreadable-runtime",
        ] {
            let root = tempfile::tempdir().unwrap();
            let user = root.path().join("user");
            let runtime = user.join(uid.to_string());
            if case != "missing-user" {
                fs::create_dir(&user).unwrap();
            }
            if !matches!(
                case,
                "missing-user" | "missing-runtime" | "runtime-link" | "user-link" | "user-file"
            ) {
                fs::create_dir(&runtime).unwrap();
            }
            match case {
                "private-file" | "private-link" => {
                    fs::create_dir(runtime.join("systemd")).unwrap();
                    if case == "private-file" {
                        fs::write(runtime.join("systemd/private"), b"not a socket").unwrap();
                    } else {
                        symlink("missing", runtime.join("systemd/private")).unwrap();
                    }
                }
                "systemd-link" => symlink("missing", runtime.join("systemd")).unwrap(),
                "runtime-link" => symlink("missing", &runtime).unwrap(),
                "user-link" => {
                    fs::remove_dir(&user).unwrap();
                    symlink("missing", &user).unwrap();
                }
                "user-file" => {
                    fs::remove_dir(&user).unwrap();
                    fs::write(&user, b"not a directory").unwrap();
                }
                "unsafe-run" => {
                    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap()
                }
                "unsafe-runtime" => {
                    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o777)).unwrap()
                }
                "unreadable-runtime" => {
                    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o0)).unwrap()
                }
                _ => {}
            }
            let _fixture = super::super::systemd::legacy_runtime_fixture::install(root.path());
            let result = if case == "wrong-owner" {
                LegacyRuntimeDirectories::open(&runtime, uid, uid.wrapping_add(1)).map(|_| false)
            } else {
                LegacyUserManager::capture("aoe-session-fixture-").map(|manager| manager.is_none())
            };
            if matches!(case, "missing-user" | "missing-runtime") {
                assert!(result.unwrap(), "{case}");
                assert!(legacy_scope_unit_names("aoe-session-fixture-")
                    .unwrap()
                    .is_empty());
                assert!(legacy_scope_inventory_clear("aoe-session-fixture-", &[]).unwrap());
            } else {
                assert!(result.is_err(), "{case} must retain unknown custody");
                if case != "wrong-owner" {
                    assert!(
                        legacy_scope_unit_names("aoe-session-fixture-").is_err(),
                        "{case}"
                    );
                    assert!(
                        legacy_scope_inventory_clear("aoe-session-fixture-", &[]).is_err(),
                        "{case}"
                    );
                }
            }
            if case == "unreadable-runtime" {
                fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
    }

    #[test]
    fn runtime_revalidation_rejects_replaced_ancestors_and_new_runtime() {
        let uid = nix::unistd::geteuid().as_raw();
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        fs::create_dir(&user).unwrap();
        let runtime = user.join(uid.to_string());
        let absent = LegacyRuntimeDirectories::open(&runtime, uid, uid).unwrap();
        fs::create_dir(&runtime).unwrap();
        assert!(absent.check_current(&runtime, uid, uid).is_err());
        let present = LegacyRuntimeDirectories::open(&runtime, uid, uid).unwrap();
        fs::rename(&user, root.path().join("old-user")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        assert!(present.check_current(&runtime, uid, uid).is_err());
    }

    #[test]
    fn manager_root_requires_the_whole_mount_and_the_physical_peer() {
        assert!(validate_legacy_cgroup_mount(
            "31 20 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n"
        )
        .is_ok());
        assert!(validate_legacy_cgroup_mount(
            "31 20 0:27 /user.slice/hidden /sys/fs/cgroup rw - cgroup2 cgroup rw\n"
        )
        .is_err());
        assert!(
            validate_legacy_cgroup_mount("31 20 0:27 / /sys/fs/cgroup rw - tmpfs tmpfs rw\n")
                .is_err()
        );
        assert!(validate_legacy_cgroup_mount(
            "31 20 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n\
             32 31 0:31 / /sys/fs/cgroup/user.slice rw - tmpfs tmpfs rw\n"
        )
        .is_err());
        for (members, valid) in [
            ("3703\n3749\n", true),
            ("3749\n", false),
            ("", false),
            ("3703\nbad\n", false),
            ("3703\n0\n", false),
        ] {
            assert_eq!(validate_legacy_manager_pid(members, 3703).is_ok(), valid);
        }
        for bad in [
            "",
            "/",
            "relative",
            "/user.slice/../other",
            "/user.slice//other",
            "/user.slice/",
        ] {
            assert!(validate_legacy_manager_root(bad).is_err(), "{bad}");
        }
        let root = "/user.slice/user-1000.slice/user@1000.service";
        assert!(validate_legacy_manager_root(root).is_ok());
        assert!(
            validate_legacy_manager_membership(&format!("0::{root}/init.scope\n"), root).is_ok()
        );
        assert!(validate_legacy_manager_membership(
            "0::/user.slice/user-2000.slice/user@2000.service/init.scope\n",
            root
        )
        .is_err());
        assert!(
            validate_legacy_manager_membership(&format!("0::{root}/other.scope\n"), root).is_err()
        );
        assert!(validate_legacy_manager_membership(
            &format!("0::{root}/init.scope\n0::{root}/init.scope\n"),
            root
        )
        .is_err());
    }
}

/// Loaded user-manager scope names starting with `prefix`.
pub(super) fn legacy_scope_unit_names(prefix: &str) -> anyhow::Result<Vec<String>> {
    let Some(manager) = LegacyUserManager::capture(prefix)? else {
        return Ok(Vec::new());
    };
    let units = manager.list_scope_units(prefix)?;
    manager.check_current()?;
    Ok(units.into_iter().map(|(unit, _)| unit).collect())
}

pub(super) fn legacy_scope_inventory_clear(
    prefix: &str,
    known: &[crate::process::worker_registry::ScopeOwnership],
) -> anyhow::Result<bool> {
    use crate::process::worker_registry::ScopeOwnership;
    use std::collections::HashSet;
    use std::time::Duration;
    let Some(manager) = LegacyUserManager::capture(prefix)? else {
        return Ok(true);
    };
    let retained: HashSet<&str> = known
        .iter()
        .filter_map(|scope| match scope {
            ScopeOwnership::Owned { unit } => Some(unit.as_str()),
            _ => None,
        })
        .collect();
    for (unit, active) in manager.list_scope_units(prefix)? {
        let (unit, active) = (unit.as_str(), active.as_str());
        if !retained.contains(unit) {
            // Inventory can prove absence, not transfer kill authority.
            if !matches!(active, "inactive" | "failed") {
                return Ok(false);
            }
            let mut show = manager.command()?;
            show.args([
                "--user",
                "show",
                "--property=LoadState",
                "--property=ActiveState",
                "--property=ControlGroup",
                unit,
            ]);
            let show = super::run_with_timeout(&mut show, Duration::from_secs(3))?
                .context("Legacy scope status timed out")?;
            anyhow::ensure!(show.status.success(), "Legacy scope status rejected");
            let mut properties = std::collections::HashMap::new();
            for line in std::str::from_utf8(&show.stdout)?.lines() {
                let (key, value) = line
                    .split_once('=')
                    .context("Malformed legacy scope property")?;
                anyhow::ensure!(
                    properties.insert(key, value).is_none(),
                    "Duplicate legacy scope property"
                );
            }
            anyhow::ensure!(
                properties.contains_key("LoadState"),
                "Missing legacy scope load state"
            );
            if !properties
                .get("ActiveState")
                .is_some_and(|state| matches!(*state, "inactive" | "failed"))
                || properties.get("ControlGroup") != Some(&"")
            {
                return Ok(false);
            }
        }
    }
    if !legacy_scope_cgroups_clear(
        open_cgroup_directory(&manager.inventory_root())?,
        prefix,
        &retained,
    )? {
        return Ok(false);
    }
    manager.check_current()?;
    Ok(true)
}

fn open_cgroup_directory(root: &Path) -> anyhow::Result<fs::File> {
    Ok(nix::fcntl::open(
        root,
        LegacyRuntimeDirectories::flags(),
        nix::sys::stat::Mode::empty(),
    )?
    .into())
}

fn require_cgroup2(directory: &fs::File) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd as _;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: directory is live and stat points to writable storage.
    anyhow::ensure!(
        unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } == 0,
        "Cannot inspect the cgroup filesystem: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fstatfs succeeded and initialized stat.
    let filesystem_type = unsafe { stat.assume_init() }.f_type;
    anyhow::ensure!(
        i128::from(filesystem_type) == i128::from(libc::CGROUP2_SUPER_MAGIC),
        "Scope inventory is not a cgroup2 filesystem"
    );
    Ok(())
}

fn read_cgroup_file(directory: &fs::File, name: &std::ffi::CStr) -> anyhow::Result<String> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    let mut file = runtime_io::open_readonly_at(directory.as_raw_fd(), name)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Cgroup interface is not a regular file"
    );
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(contents)
}

fn validate_physical_cgroup_root(directory: &fs::File) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd as _;
    for name in ["cgroup.events", "cgroup.type"] {
        match runtime_io::stat_entry(directory.as_raw_fd(), name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!("A cgroup subtree hides part of the physical hierarchy"),
        }
    }
    read_cgroup_file(directory, c"cgroup.controllers")?;
    Ok(())
}

fn kernel_scope_inventory_clear(prefix: &str) -> anyhow::Result<bool> {
    #[cfg(test)]
    if let Some(root) = super::systemd::legacy_runtime_fixture::root() {
        return kernel_scope_inventory_clear_at(&root.join("cgroups"), prefix, false);
    }
    kernel_scope_inventory_clear_at(Path::new("/sys/fs/cgroup"), prefix, true)
}

fn kernel_scope_inventory_clear_at(
    root: &Path,
    prefix: &str,
    kernel: bool,
) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let namespace = fs::read_link("/proc/self/ns/cgroup")?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let directory = open_cgroup_directory(root)?;
    if kernel {
        validate_legacy_cgroup_mount(&mountinfo)?;
        require_cgroup2(&directory)?;
    }
    // Non-root kernel cgroups expose both interfaces, even in a cgroup namespace.
    validate_physical_cgroup_root(&directory)?;
    let clear = legacy_scope_cgroups_clear(
        directory.try_clone()?,
        prefix,
        &std::collections::HashSet::new(),
    )?;
    let before = directory.metadata()?;
    let after = open_cgroup_directory(root)?.metadata()?;
    validate_physical_cgroup_root(&directory)?;
    let mount_unchanged = !kernel
        || validate_legacy_cgroup_mount(&fs::read_to_string("/proc/self/mountinfo")?)?
            == validate_legacy_cgroup_mount(&mountinfo)?;
    anyhow::ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && fs::read_link("/proc/self/ns/cgroup")? == namespace
            && mount_unchanged,
        "Kernel scope inventory boundary changed during absence proof"
    );
    Ok(clear)
}

fn legacy_scope_cgroups_clear(
    root: fs::File,
    prefix: &str,
    retained: &std::collections::HashSet<&str>,
) -> anyhow::Result<bool> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let entries = runtime_io::read_directory(root.as_raw_fd())?;
    let mut pending = vec![(root, entries)];
    while let Some((directory, entries)) = pending.last_mut() {
        let Some(entry) = entries.next() else {
            pending.pop();
            continue;
        };
        let entry = entry?;
        let kind = entry.file_type()?;
        anyhow::ensure!(!kind.is_symlink(), "Cgroup inventory contains a link");
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name();
        #[cfg(test)]
        legacy_inventory_boundary_tests::before_open(&name);
        let child: fs::File = match nix::fcntl::openat(
            &*directory,
            name.as_os_str(),
            LegacyRuntimeDirectories::flags(),
            nix::sys::stat::Mode::empty(),
        ) {
            Ok(child) => child.into(),
            Err(nix::errno::Errno::ENOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if name.as_bytes().starts_with(prefix.as_bytes())
            && name.as_bytes().ends_with(b".scope")
            && !retained.contains(name.to_str().context("Non-UTF8 matching scope name")?)
        {
            let events = read_cgroup_file(&child, c"cgroup.events")?;
            let mut populated = None;
            for line in events.lines() {
                let mut fields = line.split_whitespace();
                if fields.next() == Some("populated") {
                    let value = fields.next().context("Missing cgroup population")?;
                    anyhow::ensure!(
                        populated.is_none()
                            && fields.next().is_none()
                            && matches!(value, "0" | "1"),
                        "Invalid cgroup population"
                    );
                    populated = Some(value == "1");
                }
            }
            match populated {
                Some(false) => {}
                Some(true) => return Ok(false),
                None => anyhow::bail!("Cgroup population unavailable"),
            }
        }
        let entries = runtime_io::read_directory(child.as_raw_fd())?;
        pending.push((child, entries));
    }
    Ok(true)
}
pub(super) fn open_read_nonblocking(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}
pub(super) fn launch_executable(
    executable: std::path::PathBuf,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    if executable.try_exists()? {
        return Ok(executable);
    }
    let replacement = executable
        .as_os_str()
        .as_bytes()
        .strip_suffix(b" (deleted)")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "AoE executable is unavailable",
            )
        })?;
    which::which(Path::new(std::ffi::OsStr::from_bytes(replacement)))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::NotFound, error))
}
pub(super) fn rename_exclusive(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use nix::NixPath;
    use std::os::fd::AsRawFd;

    let result = source.with_nix_path(|source| {
        destination.with_nix_path(|destination| {
            // SAFETY: both descriptors remain owned for the call and NixPath
            // supplies live NUL-terminated names. The syscall also supports musl.
            nix::errno::Errno::result(unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    destination_dir.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            })
            .map(|_| ())
        })
    })??;
    result.map_err(std::io::Error::from)
}

pub(super) fn connect_systemd_socket(
    socket: &std::os::unix::net::UnixDatagram,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    if let Some(name) = destination.as_bytes().strip_prefix(b"@") {
        socket.connect_addr(&std::os::unix::net::SocketAddr::from_abstract_name(name)?)
    } else {
        socket.connect(destination)
    }
}

pub(crate) fn unix_peer_uid(stream: &tokio::net::UnixStream) -> std::io::Result<u32> {
    Ok(stream.peer_cred()?.uid())
}

pub(super) fn collect_pid_tree(pid: u32) -> Vec<u32> {
    let children_map = build_children_map();
    let mut pids = vec![pid];
    super::collect_descendants_from_map(pid, &children_map, &mut pids);
    pids
}

pub(super) fn build_children_map() -> HashMap<u32, Vec<u32>> {
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    let proc_dir = Path::new("/proc");
    let Ok(entries) = fs::read_dir(proc_dir) else {
        return children_map;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let Ok(child_pid) = name_str.parse::<u32>() else {
            continue;
        };

        let stat_path = entry.path().join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            continue;
        };

        if let Some(ppid) = parse_stat_field(&content, 3) {
            children_map.entry(ppid as u32).or_default().push(child_pid);
        }
    }

    children_map
}

/// One `/proc` walk deciding, for each candidate `i`, whether a live process
/// belongs to it: an `/proc/<pid>/environ` *entry* exactly equals
/// `env_needles[i]` (NUL-delimited, so no prefix-collision), and
/// `/proc/<pid>/cmdline` contains `cmdline_needles[i]` when both are supplied.
/// An executable needle matches an exact argv-token basename. A candidate with
/// one signal uses that one; otherwise every supplied signal must match.
/// `environ` is owner-only,
/// so only same-uid processes (our agent children among them) contribute an
/// environment match. Skips entries that vanish or are unreadable mid-scan;
/// stops early once every candidate is matched. Best-effort: an unreadable
/// `/proc` yields all `false`.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
) -> Vec<bool> {
    let n = env_needles.len();
    let mut found = vec![false; n];
    let mut remaining = n;
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        if remaining == 0 {
            break;
        }
        let name = entry.file_name();
        if name.to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let dir = entry.path();

        let environ_raw = fs::read(dir.join("environ")).unwrap_or_default();
        let environ = String::from_utf8_lossy(&environ_raw);
        let env_entries: std::collections::HashSet<&str> =
            environ.split('\0').filter(|s| !s.is_empty()).collect();

        let cmd_raw = fs::read(dir.join("cmdline")).unwrap_or_default();
        let cmdline_raw = String::from_utf8_lossy(&cmd_raw);
        let cmd_tokens: Vec<&str> = cmdline_raw
            .split('\0')
            .filter(|value| !value.is_empty())
            .collect();
        let cmdline = cmdline_raw.replace('\0', " ");

        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_hit =
                !env_needles[i].is_empty() && env_entries.contains(env_needles[i].as_str());
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && cmdline.contains(s));
            let executable_hit = executable_needles[i].as_deref().is_some_and(|needle| {
                !needle.is_empty()
                    && cmd_tokens.iter().any(|token| {
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
                remaining -= 1;
            }
        }
    }
    found
}

/// A process mid-exit reports pgrp `-1`; it belongs to no group.
fn parse_process_group(field: &str) -> anyhow::Result<Option<u32>> {
    let pgrp = field
        .parse::<i32>()
        .with_context(|| format!("invalid process group {field:?}"))?;
    Ok(u32::try_from(pgrp).ok())
}

fn process_disappeared(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn foreign_process_permission_error(error: &std::io::Error, owner: Option<u32>, euid: u32) -> bool {
    matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM))
        && owner.is_some_and(|uid| uid != euid)
}

pub(super) fn runner_group_executing(group: u32) -> anyhow::Result<bool> {
    use std::io::Read as _;
    let mut bytes = [0u8; 1024];
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let size = match fs::File::open(entry.path().join("stat"))
            .and_then(|mut file| file.read(&mut bytes))
        {
            Ok(size) => size,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) => {
                use std::os::unix::fs::MetadataExt as _;
                let owner = entry.metadata().ok().map(|metadata| metadata.uid());
                if foreign_process_permission_error(&error, owner, unsafe { libc::geteuid() }) {
                    continue;
                }
                return Err(error).context("runner group census unavailable");
            }
        };
        let offset = bytes[..size]
            .windows(2)
            .rposition(|pair| pair == b") ")
            .context("invalid process stat")?;
        let mut fields = std::str::from_utf8(&bytes[offset + 2..size])?.split_ascii_whitespace();
        let state = fields.next().context("missing process state")?;
        let process_group = parse_process_group(fields.nth(1).context("missing process group")?)?;
        if process_group == Some(group) && state != "Z" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn process_start_id(pid: u32) -> anyhow::Result<Option<String>> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error).context("reading process birth identity"),
    };
    let offset = stat
        .windows(2)
        .rposition(|pair| pair == b") ")
        .context("invalid process stat")?;
    let tail = std::str::from_utf8(&stat[offset + 2..])?;
    let mut fields = tail.split_ascii_whitespace();
    fields.next().context("missing process state")?;
    let start = fields.nth(18).context("missing process birth identity")?;
    Ok(Some(format!("proc:{start}")))
}

pub(super) struct ProcessHandle(std::os::fd::OwnedFd);
pub(super) fn process_group_id(pid: u32) -> anyhow::Result<Option<u32>> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let offset = stat
        .windows(2)
        .rposition(|pair| pair == b") ")
        .context("invalid process stat")?;
    let tail = std::str::from_utf8(&stat[offset + 2..])?;
    parse_process_group(
        tail.split_ascii_whitespace()
            .nth(2)
            .context("missing process group")?,
    )
}
impl ProcessHandle {
    pub(super) fn capture(identity: &super::PrimeProcessIdentity) -> anyhow::Result<Option<Self>> {
        Self::capture_identity(identity, false)
    }

    pub(super) fn capture_group(
        identity: &super::PrimeProcessIdentity,
    ) -> anyhow::Result<Option<Self>> {
        Self::capture_identity(identity, true)
    }

    fn capture_identity(
        identity: &super::PrimeProcessIdentity,
        allow_exited: bool,
    ) -> anyhow::Result<Option<Self>> {
        use std::os::fd::FromRawFd as _;
        let pid = i32::try_from(identity.pid)?;
        anyhow::ensure!(pid > 0, "invalid owned process PID");
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            let error = std::io::Error::last_os_error();
            if process_disappeared(&error) {
                return Ok(None);
            }
            return Err(error).context("opening owned process handle");
        }
        let handle = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) });
        if process_start_id(identity.pid)?.as_deref() != Some(&identity.start_id)
            || (!allow_exited && handle.exited()?)
        {
            return Ok(None);
        }
        Ok(Some(handle))
    }
    pub(super) fn group_executing(&self) -> anyhow::Result<Option<bool>> {
        match self.send_group(0) {
            Ok(()) => Ok(Some(true)),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(Some(false)),
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn signal_group(&self, signal: i32) -> anyhow::Result<()> {
        match self.send_group(signal) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn send_group(&self, signal: i32) -> std::io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // PIDFD_SIGNAL_PROCESS_GROUP binds the original group, including after leader reap.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                4,
            )
        };
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub(super) fn exited(&self) -> anyhow::Result<bool> {
        use std::os::fd::AsRawFd as _;
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("checking owned process exit");
        }
        Ok(result > 0)
    }

    pub(super) fn signal(&self, signal: i32) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd as _;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if !process_disappeared(&error) {
                return Err(error).context("signalling owned process handle");
            }
        }
        Ok(())
    }
}

pub(super) fn runner_group_identities(
    group: u32,
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut result = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) => {
                let owner = entry.metadata().ok().map(|metadata| metadata.uid());
                if foreign_process_permission_error(&error, owner, unsafe { libc::geteuid() }) {
                    continue;
                }
                return Err(error).context("inspecting owned runner group");
            }
        };
        let (_, tail) = stat.rsplit_once(") ").context("invalid process stat")?;
        let mut fields = tail.split_ascii_whitespace();
        let state = fields.next().context("missing process state")?;
        let pgrp = parse_process_group(fields.nth(1).context("missing process group")?)?;
        if state != "Z" && pgrp == Some(group) {
            let start = fields.nth(16).context("missing process birth identity")?;
            result.push(super::PrimeProcessIdentity {
                pid,
                start_id: format!("proc:{start}"),
            });
        }
    }
    Ok(result)
}

pub(super) fn process_tree_samples(
    root: &super::PrimeProcessIdentity,
) -> anyhow::Result<Vec<super::PrimeProcessSample>> {
    let mut samples = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) if pid == root.pid => return Err(error.into()),
            Err(_) => continue,
        };
        let (_, tail) = stat.rsplit_once(") ").context("invalid process stat")?;
        let mut fields = tail.split_ascii_whitespace();
        let state = fields.next().context("missing process state")?;
        let parent = fields.next().context("missing process parent")?.parse()?;
        let start = fields.nth(17).context("missing process start identity")?;
        if state != "Z" {
            samples.push(super::PrimeProcessSample {
                identity: super::PrimeProcessIdentity {
                    pid,
                    start_id: format!("proc:{start}"),
                },
                parent,
                marked: false,
            });
        }
    }
    anyhow::ensure!(
        samples.iter().any(|sample| sample.identity == *root),
        "tool root omitted from census"
    );
    Ok(samples)
}

pub(super) fn prime_processes(
    marker: &str,
    socket: &Path,
    known: &[super::PrimeProcessIdentity],
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let mut samples = Vec::new();
    let socket = socket.as_os_str().as_encoded_bytes();
    let read = |path: &Path, bytes: &mut Vec<u8>| -> std::io::Result<()> {
        bytes.clear();
        fs::File::open(path)?.read_to_end(bytes)?;
        Ok(())
    };
    fs::metadata("/proc/self/stat").context("checking the Prime process census")?;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let mut path = entry.path().join("stat");
        if let Err(error) = read(&path, &mut bytes) {
            if !process_disappeared(&error) && known.iter().any(|identity| identity.pid == pid) {
                return Err(error).with_context(|| format!("inspecting owned Prime process {pid}"));
            }
            continue;
        }
        let offset = bytes
            .windows(2)
            .rposition(|pair| pair == b") ")
            .context("invalid process stat")?;
        let tail = std::str::from_utf8(&bytes[offset + 2..])?;
        let mut fields = tail.split_ascii_whitespace();
        let state = fields.next().context("missing process state")?;
        let parent = fields.next().context("missing process parent")?.parse()?;
        let start = fields.nth(17).context("missing process start identity")?;
        if state == "Z" {
            continue;
        }
        let identity = super::PrimeProcessIdentity {
            pid,
            start_id: format!("proc:{start}"),
        };
        path.set_file_name("environ");
        let mut marked = read(&path, &mut bytes).is_ok()
            && bytes
                .split(|byte| *byte == 0)
                .any(|entry| entry == marker.as_bytes());
        if !marked {
            path.set_file_name("cmdline");
            marked = read(&path, &mut bytes).is_ok()
                && bytes.split(|byte| *byte == 0).any(|entry| entry == socket);
        }
        samples.push(super::PrimeProcessSample {
            identity,
            parent,
            marked,
        });
    }
    for identity in known {
        if !samples
            .iter()
            .any(|sample| sample.identity.pid == identity.pid)
        {
            let path = Path::new("/proc")
                .join(identity.pid.to_string())
                .join("stat");
            match read(&path, &mut bytes) {
                Ok(()) => anyhow::ensure!(
                    bytes
                        .windows(2)
                        .rposition(|pair| pair == b") ")
                        .is_some_and(|offset| bytes[offset + 2..].starts_with(b"Z ")),
                    "owned Prime process {} omitted from census",
                    identity.pid
                ),
                Err(error) if process_disappeared(&error) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspecting owned Prime process {}", identity.pid)
                    })
                }
            }
        }
    }
    super::select_stable_prime_processes(samples, known)
}

/// Sample system memory from `/proc` and `/sys`: a handful of small pseudo-file
/// reads, cheap enough for the background sampler. Any file that is missing or
/// unparseable leaves its field at the default (0 for the always-present RAM
/// figures, `None` for the optional ones) so a transient read never fabricates
/// a reading.
pub(super) fn sample_memory() -> super::metrics::MemorySample {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let total = parse_meminfo_field(&meminfo, "MemTotal").map(kib_to_bytes);
    let avail = parse_meminfo_field(&meminfo, "MemAvailable").map(kib_to_bytes);

    let psi_mem_some_avg10 =
        parse_psi_some_avg10(&fs::read_to_string("/proc/pressure/memory").unwrap_or_default());
    let psi_io_some_avg10 =
        parse_psi_some_avg10(&fs::read_to_string("/proc/pressure/io").unwrap_or_default());

    // Old kernels and WSL1 omit MemAvailable; report unknown rather than a false 100% used.
    let (total_bytes, available_bytes) = match (total, avail) {
        (Some(t), Some(a)) => (t, a),
        _ => (0, 0),
    };

    super::metrics::MemorySample {
        total_bytes,
        available_bytes,
        psi_mem_some_avg10,
        psi_io_some_avg10,
        macos_pressure_level: None,
    }
}

pub(super) fn sample_system() -> super::metrics::SystemReading {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let cpu = stat.lines().next().and_then(|line| {
        let values: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|value| value.parse().ok())
            .collect();
        (!values.is_empty()).then(|| {
            (
                values.iter().sum(),
                values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0),
            )
        })
    });
    let load = fs::read_to_string("/proc/loadavg").ok().and_then(|value| {
        let values: Vec<f64> = value
            .split_whitespace()
            .take(3)
            .filter_map(|part| part.parse().ok())
            .collect();
        (values.len() == 3).then(|| [values[0], values[1], values[2]])
    });
    let mem = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        mem.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let total = field("SwapTotal:");
    let free = field("SwapFree:");
    (cpu, None, load, (total, total.saturating_sub(free)))
}

pub(super) fn process_snapshot() -> Vec<super::metrics::ProcessRecord> {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let end = stat.rfind(')')?;
            let fields: Vec<&str> = stat[end + 2..].split_whitespace().collect();
            let ppid = fields.get(1)?.parse().ok()?;
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            let start_id = fields.get(19)?.parse().ok()?;
            let rss_pages: i64 = fields.get(21)?.parse().ok()?;
            Some(super::metrics::ProcessRecord {
                pid,
                ppid,
                start_id,
                rss_bytes: rss_pages.max(0) as u64 * page,
                cpu_seconds: (utime + stime) as f64 / hz,
            })
        })
        .collect()
}

fn kib_to_bytes(kib: u64) -> u64 {
    kib.saturating_mul(1024)
}

fn parse_meminfo_field(meminfo: &str, key: &str) -> Option<u64> {
    for line in meminfo.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

/// `None` when PSI is unavailable, never a false 0.0.
fn parse_psi_some_avg10(psi: &str) -> Option<f32> {
    for line in psi.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("some") {
            continue;
        }
        return fields.find_map(|kv| kv.strip_prefix("avg10=")?.parse().ok());
    }
    None
}

/// Per-boot identity from `/proc/sys/kernel/random/boot_id`: constant for the
/// boot's lifetime and immune to clock changes (the property the ledger needs).
pub(super) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let stat_path = format!("/proc/{}/stat", shell_pid);
    let stat_content = fs::read_to_string(&stat_path).ok()?;

    // Parse stat: pid (comm) state ppid pgrp session tty_nr tpgid ...
    // tpgid (field 8, 0-indexed 7) is the foreground process group ID
    let tpgid = parse_stat_field(&stat_content, 7)?;

    if tpgid <= 0 {
        return Some(shell_pid);
    }

    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

fn find_process_in_group(pgrp: u32) -> Option<u32> {
    let proc_dir = Path::new("/proc");
    if !proc_dir.exists() {
        return None;
    }

    // A process can exit mid-scan; skip it rather than abort to the shell pid.
    for entry in fs::read_dir(proc_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };

        let stat_path = entry.path().join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            continue;
        };

        // Field 5 (0-indexed 4) is the process group ID
        if let Some(proc_pgrp) = parse_stat_field(&content, 4) {
            if proc_pgrp as u32 == pgrp {
                return Some(pid);
            }
        }
    }

    None
}

pub(super) fn foreground_arguments(shell_pid: u32) -> Vec<super::ProcessArguments> {
    use std::io::Read;
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
            let mut bytes = Vec::new();
            fs::File::open(format!("/proc/{pid}/cmdline"))
                .ok()?
                .take(256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 256 * 1024 {
                return None;
            }
            let argv: Vec<String> = bytes
                .strip_suffix(&[0])
                .unwrap_or(&bytes)
                .split(|byte| *byte == 0)
                .map(|word| std::str::from_utf8(word).map(str::to_owned))
                .collect::<Result<_, _>>()
                .ok()?;

            let stdin = fs::read_link(format!("/proc/{pid}/fd/0")).ok();
            let stdin_is_tty = stdin.map(|path| {
                path.starts_with("/dev/pts")
                    || path == std::path::Path::new("/dev/tty")
                    || path == std::path::Path::new("/dev/console")
                    || path.to_string_lossy().starts_with("/dev/tty")
            });
            Some(super::ProcessArguments { argv, stdin_is_tty })
        })
        .collect()
}

pub(super) fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ppid = u32::try_from(parse_stat_field(&stat, 3)?).ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    Some((ppid, String::from_utf8_lossy(argv0).into_owned()))
}

/// `comm` (field 2) may contain spaces.
fn parse_stat_field(content: &str, field_idx: usize) -> Option<i64> {
    let close_paren = content.rfind(')')?;
    let after_comm = &content[close_paren + 2..]; // Skip ") "

    // Fields after comm start at index 2 (state is index 2)
    // So field_idx 4 means we want the 3rd field after comm (index 2 in after_comm split)
    let adjusted_idx = field_idx.checked_sub(2)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(adjusted_idx)?.parse().ok()
}

/// Prevents user-idle system sleep by holding a `systemd-inhibit` block lock.
/// `--what=idle:sleep` blocks idle sleep only (the display still sleeps).
pub(super) struct SystemdInhibitor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl SystemdInhibitor {
    pub(super) fn new() -> Self {
        Self {
            child: None,
            stdin: None,
        }
    }
}

impl super::SleepInhibit for SystemdInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        let mut child = match Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                "--mode=block",
                "--who=Agent of Empires",
                "--why=Active agent sessions",
                "cat",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::latch_sleep_inhibit_unavailable(
                    "systemd-inhibit not found; OS sleep will not be inhibited on this host",
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // `systemd-inhibit` holds the lock only while `cat` runs, which ends at stdin EOF.
        self.stdin = child.stdin.take();
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        // logind releases the lock when the holder dies by any cause.
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn is_held_alive(&mut self) -> bool {
        super::sleep_inhibit_child_held_alive(
            &mut self.child,
            "systemd-inhibit exited without taking the lock (no logind?); \
             OS sleep will not be inhibited on this host",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_group_field_tolerates_a_process_mid_exit() {
        assert_eq!(parse_process_group("-1").unwrap(), None);
        assert_eq!(parse_process_group("42").unwrap(), Some(42));
        let error = parse_process_group("S").unwrap_err();
        assert!(format!("{error:#}").contains("\"S\""));
    }

    #[test]
    fn launch_executable_recovers_only_deleted_paths_with_a_live_replacement() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("aoe");
        let deleted = directory.path().join("aoe (deleted)");
        assert!(launch_executable(deleted.clone()).is_err());
        fs::write(&installed, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(launch_executable(deleted.clone()).unwrap(), installed);
        fs::write(&deleted, "#!/bin/sh\n").unwrap();
        assert_eq!(launch_executable(deleted.clone()).unwrap(), deleted);
        fs::remove_file(&deleted).unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(launch_executable(deleted).is_err());
    }

    #[test]
    fn reaped_prime_stat_is_absence_but_other_read_errors_are_not() {
        use std::io::Read as _;
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stat = fs::File::open(format!("/proc/{}/stat", child.id())).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        let error = stat.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert!(process_disappeared(&error));
        for (errno, gone) in [
            (libc::ENOENT, true),
            (libc::EACCES, false),
            (libc::EIO, false),
        ] {
            assert_eq!(
                process_disappeared(&std::io::Error::from_raw_os_error(errno)),
                gone
            );
        }
    }

    #[test]
    fn group_census_ignores_only_proven_foreign_permission_errors() {
        for (errno, owner, expected) in [
            (libc::EACCES, Some(1001), true),
            (libc::EPERM, Some(1001), true),
            (libc::EACCES, Some(1000), false),
            (libc::EPERM, None, false),
            (libc::EIO, Some(1001), false),
        ] {
            assert_eq!(
                foreign_process_permission_error(
                    &std::io::Error::from_raw_os_error(errno),
                    owner,
                    1000
                ),
                expected
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn prime_quiescence_never_declares_an_uninspectable_owned_process_absent() {
        use std::io::{BufRead as _, Read as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;
        if std::env::var_os("AOE_PRIME_CENSUS_CHILD").is_some() {
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_NAME, c"\xffowned".as_ptr(), 0, 0, 0) },
                0
            );
            let signal = |message: &[u8]| {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(message).unwrap();
                stdout.flush().unwrap();
            };
            signal(b"warm-ready\n");
            std::io::stdin().read_exact(&mut [0]).unwrap();
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }, 0);
            signal(b"census-ready\n");
            std::io::stdin().read_exact(&mut [0]).unwrap();
            return;
        }
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        struct OwnedRuntime(crate::process::prime::PrimeRuntime);
        impl Drop for OwnedRuntime {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0.directory);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let program = root.path().join("native-shutdown");
        fs::write(&program, "#!/bin/sh\nprintf '%s' '{\"failed\":[]}'\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = OwnedRuntime(
            crate::process::prime::PrimeRuntime::new(program, root.path().join("agent"), None)
                .unwrap(),
        );
        runtime.0.prepare().unwrap();
        let cleanup = runtime.0.directory.join("cleanup");
        let original = runtime.0.directory.join("cleanup.original");
        fs::rename(&cleanup, &original).unwrap();
        std::os::unix::fs::symlink(root.path(), &cleanup).unwrap();
        assert!(runtime.0.cleanup().is_err());
        assert!(runtime.0.directory.is_dir());
        fs::remove_file(&cleanup).unwrap();
        fs::rename(original, cleanup).unwrap();
        let route = serde_json::to_string(&runtime.0).unwrap();
        let proof_path = crate::process::worker_registry::workers_dir()
            .unwrap()
            .join(format!(
                ".{}.prime-proof",
                runtime.0.directory.file_name().unwrap().to_str().unwrap()
            ));
        let mut receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&proof_path).unwrap()).unwrap();
        receipt["settled"] = serde_json::json!(true);
        fs::write(&proof_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        let name = format!(
            "{}::prime_quiescence_never_declares_an_uninspectable_owned_process_absent",
            module_path!().split_once("::").unwrap().1
        );
        let mut child = OwnedChild(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env("AOE_PRIME_CENSUS_CHILD", "1")
                .env("TMPDIR", &runtime.0.directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut reader = std::io::BufReader::new(child.0.stdout.take().unwrap());
        let mut ready = |expected: &str| {
            let mut line = String::new();
            loop {
                assert!(
                    reader.read_line(&mut line).unwrap() != 0,
                    "owned probe exited before readiness"
                );
                if line.trim() == expected {
                    break;
                }
                line.clear();
            }
        };
        ready("warm-ready");
        let stat = fs::read(format!("/proc/{}/stat", child.0.id())).unwrap();
        let offset = stat.windows(2).rposition(|pair| pair == b") ").unwrap();
        let start = std::str::from_utf8(&stat[offset + 2..])
            .unwrap()
            .split_ascii_whitespace()
            .nth(19)
            .unwrap();
        let known = super::super::PrimeProcessIdentity {
            pid: child.0.id(),
            start_id: format!("proc:{start}"),
        };
        assert!(runtime.0.cleanup().is_err());
        child.0.stdin.as_mut().unwrap().write_all(&[0]).unwrap();
        ready("census-ready");
        assert!(child.0.try_wait().unwrap().is_none());
        let marker = format!("TMPDIR={}", runtime.0.directory.display());
        let unbound = prime_processes(&marker, &runtime.0.socket(), &[]).unwrap();
        assert!(
            !unbound.iter().any(|identity| identity.pid == child.0.id()),
            "UID alone assigned foreign ownership"
        );
        let owned =
            prime_processes(&marker, &runtime.0.socket(), std::slice::from_ref(&known)).unwrap();
        assert!(
            owned.contains(&known),
            "unreadable owned environment was treated as absence"
        );
        let fresh: crate::process::prime::PrimeRuntime = serde_json::from_str(&route).unwrap();
        fresh.cleanup().unwrap();
        assert!(
            !child.0.wait().unwrap().success(),
            "the attested process must actually exit, not merely disappear from the census"
        );
        let fresh: crate::process::prime::PrimeRuntime = serde_json::from_str(&route).unwrap();
        fresh.cleanup().unwrap();
        assert!(!runtime.0.directory.exists());
    }

    #[test]
    fn test_parse_stat_field() {
        let stat = "1234 (bash) S 1233 1234 1234 34816 1234 4194304 1234 0 0 0";
        // Fields: pid(0) comm(1) state(2) ppid(3) pgrp(4) session(5) tty(6) tpgid(7) ...

        assert_eq!(parse_stat_field(stat, 3), Some(1233)); // ppid
        assert_eq!(parse_stat_field(stat, 4), Some(1234)); // pgrp
        assert_eq!(parse_stat_field(stat, 7), Some(1234)); // tpgid
    }

    const MEMINFO: &str = "\
MemTotal:       32791036 kB
MemFree:         1234567 kB
MemAvailable:    9876543 kB
Cached:          5678901 kB
";

    #[test]
    fn test_parse_meminfo_field() {
        let cases = [
            ("MemTotal", Some(32791036)),
            ("MemAvailable", Some(9876543)),
            ("MemFree", Some(1234567)),
            ("Nonexistent", None),
            ("Mem", None),
        ];
        for (key, expected) in cases {
            assert_eq!(parse_meminfo_field(MEMINFO, key), expected, "{key}");
        }
    }

    #[test]
    fn test_parse_psi_some_avg10() {
        let psi = "\
some avg10=1.23 avg60=4.56 avg300=7.89 total=123456789
full avg10=0.10 avg60=0.20 avg300=0.30 total=42
";
        assert_eq!(parse_psi_some_avg10(psi), Some(1.23));
        assert_eq!(parse_psi_some_avg10(""), None);
        assert_eq!(parse_psi_some_avg10("full avg10=5.0 total=9"), None);
    }
}

pub(super) fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}
pub(super) fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

pub(crate) mod runtime_io {
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io::{self, Write};
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::path::{Path, PathBuf};

    pub(crate) const REGULAR: u32 = libc::S_IFREG;
    pub(crate) const DIRECTORY: u32 = libc::S_IFDIR;
    pub(crate) const SOCKET: u32 = libc::S_IFSOCK;
    pub(crate) const KIND_MASK: u32 = libc::S_IFMT;
    #[cfg(test)]
    pub(crate) const STICKY: u32 = libc::S_ISVTX;
    pub(crate) enum DirectoryOpenFailure {
        Symlink,
        NotDirectory,
        Permission,
        Other,
    }
    pub(crate) fn directory_open_failure(error: &io::Error) -> DirectoryOpenFailure {
        match error.raw_os_error() {
            Some(libc::ELOOP) => DirectoryOpenFailure::Symlink,
            Some(libc::ENOTDIR) => DirectoryOpenFailure::NotDirectory,
            Some(libc::EACCES) | Some(libc::EPERM) => DirectoryOpenFailure::Permission,
            _ => DirectoryOpenFailure::Other,
        }
    }
    pub(crate) fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[derive(Clone, Copy)]
    pub(crate) struct Stat {
        pub st_mode: u32,
        pub st_uid: u32,
        pub st_dev: u64,
        pub st_ino: u64,
        pub st_nlink: libc::nlink_t,
    }
    fn stat_value(stat: libc::stat) -> Stat {
        Stat {
            st_mode: stat.st_mode,
            st_uid: stat.st_uid,
            st_dev: stat.st_dev,
            st_ino: stat.st_ino,
            st_nlink: stat.st_nlink,
        }
    }
    fn owned(fd: RawFd) -> io::Result<OwnedFd> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }
    fn name(value: &str) -> io::Result<CString> {
        CString::new(value).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }
    #[derive(Clone, Copy)]
    pub(crate) enum DirectoryAccess {
        Search,
        Read,
    }

    pub(crate) fn open_directory(path: &Path, access: DirectoryAccess) -> io::Result<OwnedFd> {
        use nix::NixPath;
        path.with_nix_path(|path| open_directory_at(libc::AT_FDCWD, path, access))?
    }
    pub(crate) fn open_directory_at(
        dir: RawFd,
        name: &CStr,
        access: DirectoryAccess,
    ) -> io::Result<OwnedFd> {
        let flags = libc::O_DIRECTORY
            | libc::O_CLOEXEC
            | match access {
                DirectoryAccess::Search => libc::O_PATH,
                DirectoryAccess::Read => libc::O_RDONLY,
            };
        owned(unsafe { libc::openat(dir, name.as_ptr(), flags) })
    }
    pub(crate) fn open_readonly_at(dir: RawFd, name: &CStr) -> io::Result<File> {
        owned(unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
        .map(File::from)
    }
    pub(crate) fn open_lock_at(dir: RawFd, name: &CStr) -> io::Result<File> {
        owned(unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })
        .map(File::from)
    }
    pub(crate) fn stat_file(fd: RawFd) -> io::Result<Stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_value(unsafe { stat.assume_init() }))
    }
    pub(crate) fn stat_entry(dir: RawFd, value: &str) -> io::Result<Stat> {
        let value = name(value)?;
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                dir,
                value.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_value(unsafe { stat.assume_init() }))
    }
    pub(crate) fn set_file_mode(fd: RawFd, mode: u32) -> io::Result<()> {
        if unsafe { libc::fchmod(fd, mode) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(crate) fn set_entry_mode(dir: RawFd, value: &str, mode: u32) -> io::Result<()> {
        let value = name(value)?;
        if unsafe { libc::fchmodat(dir, value.as_ptr(), mode, 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(crate) fn unlink_entry(dir: RawFd, value: &str) -> io::Result<()> {
        let value = name(value)?;
        if unsafe { libc::unlinkat(dir, value.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    }
    pub(crate) fn sync_directory(dir: RawFd) {
        let _ = unsafe { libc::fsync(dir) };
    }
    pub(crate) fn anchored_path(dir: RawFd, child: &str) -> io::Result<PathBuf> {
        let base = PathBuf::from(format!("/proc/self/fd/{dir}"));
        if !base.exists() {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(base.join(child))
    }
    pub(crate) fn read_directory(dir: RawFd) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(anchored_path(dir, ".")?)
    }
    pub(crate) fn write_atomic(
        dir: RawFd,
        final_name: &str,
        temporary: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let temporary_name = name(temporary)?;
        let final_name = name(final_name)?;
        let fd = unsafe {
            libc::openat(
                dir,
                temporary_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        let mut file = File::from(owned(fd)?);
        let result = (|| {
            set_file_mode(file.as_raw_fd(), 0o600)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            if unsafe { libc::renameat(dir, temporary_name.as_ptr(), dir, final_name.as_ptr()) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        drop(file);
        if result.is_err() {
            let _ = unlink_entry(dir, temporary);
        }
        result
    }
    pub(crate) struct PeerCredentials {
        pub uid: u32,
        pub pid: u32,
    }
    pub(crate) fn peer_credentials(fd: RawFd) -> io::Result<PeerCredentials> {
        let mut credentials = MaybeUninit::<libc::ucred>::uninit();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let credentials = unsafe { credentials.assume_init() };
        Ok(PeerCredentials {
            uid: credentials.uid,
            pid: credentials.pid as u32,
        })
    }
    pub(crate) fn boot_id() -> Option<String> {
        super::boot_id().filter(|value| valid_boot_id(value))
    }
    pub(crate) enum ProcessStart {
        Ticks(String),
        Absent,
        Unprovable,
    }
    #[cfg(test)]
    static PROC_READ_UNPROVABLE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    #[cfg(test)]
    pub(crate) fn fail_next_proc_read() {
        PROC_READ_UNPROVABLE.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub(crate) fn process_start_ticks(pid: u32) -> ProcessStart {
        #[cfg(test)]
        if PROC_READ_UNPROVABLE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return ProcessStart::Unprovable;
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return ProcessStart::Absent,
            Err(_) => return ProcessStart::Unprovable,
        };
        let Some(close) = stat.rfind(')') else {
            return ProcessStart::Unprovable;
        };
        let mut fields = stat[close + 1..].split_whitespace();
        if matches!(fields.next(), Some("Z" | "X")) {
            return ProcessStart::Absent;
        }
        match fields
            .nth(18)
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        {
            Some(ticks) => ProcessStart::Ticks(ticks.to_owned()),
            None => ProcessStart::Unprovable,
        }
    }
    pub(crate) fn process_start_identity(pid: u32) -> Option<String> {
        let boot = boot_id()?;
        let ProcessStart::Ticks(ticks) = process_start_ticks(pid) else {
            return None;
        };
        Some(format!("linux:v1:{boot}:{ticks}"))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ProcessIdentityState {
        Live,
        Dead,
        Unprovable,
    }

    fn valid_boot_id(value: &str) -> bool {
        value.len() == 36
            && value.bytes().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
            })
    }

    fn parse_process_identity(value: &str) -> Option<(&str, &str)> {
        let (boot, ticks) = value.strip_prefix("linux:v1:")?.split_once(':')?;
        (valid_boot_id(boot)
            && !ticks.is_empty()
            && ticks.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some((boot, ticks))
    }

    pub(crate) fn valid_process_start_identity(value: &str) -> bool {
        parse_process_identity(value).is_some()
    }

    pub(crate) fn process_identity_state(pid: u32, recorded: &str) -> ProcessIdentityState {
        let Some((recorded_boot, recorded_ticks)) = parse_process_identity(recorded) else {
            return ProcessIdentityState::Unprovable;
        };
        let Some(boot) = boot_id() else {
            return ProcessIdentityState::Unprovable;
        };
        if boot != recorded_boot {
            return ProcessIdentityState::Dead;
        }
        match process_start_ticks(pid) {
            ProcessStart::Ticks(ticks) if ticks == recorded_ticks => ProcessIdentityState::Live,
            ProcessStart::Ticks(_) | ProcessStart::Absent => ProcessIdentityState::Dead,
            ProcessStart::Unprovable => ProcessIdentityState::Unprovable,
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum AclError {
        NamedWrite,
        Unverifiable,
    }
    pub(crate) const ACL_VERSION: u32 = 2;
    pub(crate) const ACL_ENTRY_LEN: usize = 8;
    pub(crate) const ACL_USER_OBJ: u8 = 0x01;
    pub(crate) const ACL_USER: u8 = 0x02;
    pub(crate) const ACL_GROUP_OBJ: u8 = 0x04;
    pub(crate) const ACL_GROUP: u8 = 0x08;
    pub(crate) const ACL_MASK: u8 = 0x10;
    pub(crate) const ACL_OTHER: u8 = 0x20;
    pub(crate) const ACL_WRITE: u8 = 0x02;
    pub(crate) const ACL_PERMISSION_MASK: u8 = 0x07;
    pub(crate) fn validate_posix_acl(fd: RawFd, access: DirectoryAccess) -> Result<(), AclError> {
        let name = c"system.posix_acl_access";
        // O_PATH needs a pathname operation, anchored to the retained descriptor.
        let path =
            matches!(access, DirectoryAccess::Search).then(|| format!("/proc/self/fd/{fd}\0"));
        let read = |buffer: *mut libc::c_void, length: usize| unsafe {
            match &path {
                Some(path) => libc::getxattr(path.as_ptr().cast(), name.as_ptr(), buffer, length),
                None => libc::fgetxattr(fd, name.as_ptr(), buffer, length),
            }
        };
        let needed = read(std::ptr::null_mut(), 0);
        if needed < 0 {
            return match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENODATA | libc::ENOTSUP) => Ok(()),
                _ => Err(AclError::Unverifiable),
            };
        }
        let mut value = vec![0u8; needed as usize];
        let actual = read(value.as_mut_ptr().cast(), value.len());
        if actual < 0 {
            return Err(AclError::Unverifiable);
        }
        value.truncate(actual as usize);
        validate_acl_value(&value)
    }
    pub(crate) fn validate_acl_value(value: &[u8]) -> Result<(), AclError> {
        let header = value.get(..4).ok_or(AclError::Unverifiable)?;
        if u32::from_le_bytes(header.try_into().unwrap()) != ACL_VERSION {
            return Err(AclError::Unverifiable);
        }
        let (entries, remainder) = value[4..].as_chunks::<ACL_ENTRY_LEN>();
        if !remainder.is_empty() {
            return Err(AclError::Unverifiable);
        }
        for entry in entries {
            let tag = u16::from_le_bytes([entry[0], entry[1]]);
            let permissions = u16::from_le_bytes([entry[2], entry[3]]);
            let Ok(tag) = u8::try_from(tag) else {
                return Err(AclError::Unverifiable);
            };
            if !matches!(
                tag,
                ACL_USER_OBJ | ACL_USER | ACL_GROUP_OBJ | ACL_GROUP | ACL_MASK | ACL_OTHER
            ) || permissions & !u16::from(ACL_PERMISSION_MASK) != 0
            {
                return Err(AclError::Unverifiable);
            }
            if matches!(tag, ACL_USER | ACL_GROUP) && permissions & u16::from(ACL_WRITE) != 0 {
                return Err(AclError::NamedWrite);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_acl(dir: &Path, value: &[u8]) {
        use std::os::unix::ffi::OsStrExt;

        let path = CString::new(dir.as_os_str().as_bytes()).expect("path");
        let name = c"system.posix_acl_access";
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(
            result,
            0,
            "set the access ACL: {}",
            io::Error::last_os_error()
        );
    }

    #[cfg(test)]
    pub(crate) fn read_acl(dir: &Path) -> Vec<u8> {
        let file = File::open(dir).expect("open the directory");
        let name = c"system.posix_acl_access";
        let needed =
            unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        assert!(needed > 0, "the directory must carry the access ACL");
        let mut value = vec![0u8; needed as usize];
        let read = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert!(read > 0, "read the access ACL back");
        value.truncate(read as usize);
        value
    }

    #[cfg(test)]
    pub(crate) struct Umask {
        previous: libc::mode_t,
        env_lock: Option<std::sync::MutexGuard<'static, ()>>,
    }

    #[cfg(test)]
    impl Umask {
        pub(crate) fn set(mask: libc::mode_t) -> Self {
            let env_lock = crate::test_env_lock::acquire_env_lock(|| {
                eprintln!(
                    "waiting for the environment lock: another test holds a process-wide state"
                )
            });
            Self {
                previous: unsafe { libc::umask(mask) },
                env_lock,
            }
        }
    }

    #[cfg(test)]
    impl Drop for Umask {
        fn drop(&mut self) {
            unsafe { libc::umask(self.previous) };
            crate::test_env_lock::release_env_lock(self.env_lock.is_some());
        }
    }
}

#[cfg(test)]
mod runtime_identity_tests {
    use super::runtime_io::*;

    #[test]
    #[serial_test::serial]
    fn process_identities_distinguish_live_dead_and_unprovable() {
        let pid = std::process::id();
        let live = process_start_identity(pid).expect("current process identity");
        assert!(valid_process_start_identity(&live));
        assert_eq!(
            process_identity_state(pid, &live),
            ProcessIdentityState::Live
        );

        let (prefix, ticks) = live.rsplit_once(':').unwrap();
        let other_ticks = if ticks == "0" { "1" } else { "0" };
        assert_eq!(
            process_identity_state(pid, &format!("{prefix}:{other_ticks}")),
            ProcessIdentityState::Dead
        );
        let mut other_boot = boot_id().unwrap();
        let first = if other_boot.starts_with('0') {
            "1"
        } else {
            "0"
        };
        other_boot.replace_range(..1, first);
        assert_eq!(
            process_identity_state(pid, &format!("linux:v1:{other_boot}:1")),
            ProcessIdentityState::Dead
        );
        for invalid in [
            format!("darwin:v1:{other_boot}:1"),
            format!("linux:v2:{other_boot}:1"),
            format!("linux:v1:{other_boot}:123 "),
            format!("linux:v1:{other_boot}:١"),
            "linux:v1:AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE:1".into(),
            String::new(),
        ] {
            assert_eq!(
                process_identity_state(pid, &invalid),
                ProcessIdentityState::Unprovable,
                "{invalid:?}"
            );
        }
        fail_next_proc_read();
        assert_eq!(
            process_identity_state(pid, &live),
            ProcessIdentityState::Unprovable
        );
    }

    #[test]
    #[serial_test::serial]
    fn stopped_publishers_remain_live_but_unreaped_zombies_are_dead() {
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        struct Reap(Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn observe_without_reaping(pid: u32, event: libc::c_int) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
                // WNOWAIT establishes the transition without releasing the PID.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid,
                        info.as_mut_ptr(),
                        event | libc::WNOWAIT | libc::WNOHANG,
                    )
                };
                if result == -1 {
                    let error = std::io::Error::last_os_error();
                    assert_eq!(error.raw_os_error(), Some(libc::EINTR), "{error}");
                } else if unsafe { info.assume_init().si_pid() } == pid as libc::pid_t {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "process {pid} did not change state"
                );
                std::thread::yield_now();
            }
        }

        let mut child = Reap(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let identity = process_start_identity(pid).unwrap();
        let birth = crate::process::process_birth_identity(pid)
            .unwrap()
            .unwrap();
        assert!(crate::process::process_birth_is_live(pid, &birth).unwrap());
        let mut stale = birth.clone();
        stale.start_id.push_str("-reused");
        assert!(!crate::process::process_birth_is_live(pid, &stale).unwrap());
        assert_eq!(
            process_identity_state(pid, &identity),
            ProcessIdentityState::Live
        );
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) }, 0);
        observe_without_reaping(pid, libc::WSTOPPED);
        assert_eq!(
            process_identity_state(pid, &identity),
            ProcessIdentityState::Live
        );
        assert!(crate::process::process_birth_is_live(pid, &birth).unwrap());

        drop(child.0.stdin.take());
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) }, 0);
        observe_without_reaping(pid, libc::WEXITED);
        assert_eq!(
            process_identity_state(pid, &identity),
            ProcessIdentityState::Dead
        );
        assert_eq!(
            crate::process::process_birth_identity(pid).unwrap(),
            Some(birth.clone())
        );
        assert!(!crate::process::process_birth_is_live(pid, &birth).unwrap());
        child.0.wait().unwrap();
    }
}
