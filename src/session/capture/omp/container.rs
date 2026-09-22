//! In-container OMP breadcrumb capture through a POSIX sh script.

use super::*;

pub(super) fn container_exec_command(
    container_name: &str,
    runtime_name: Option<crate::session::config::ContainerRuntimeName>,
    argv: &[&str],
) -> std::process::Command {
    use crate::session::config::ContainerRuntimeName;

    let runtime = match runtime_name {
        Some(ContainerRuntimeName::AppleContainer) => {
            crate::containers::ContainerRuntime::apple_container()
        }
        Some(ContainerRuntimeName::Docker) => crate::containers::ContainerRuntime::docker(),
        Some(ContainerRuntimeName::Podman) => crate::containers::ContainerRuntime::podman(),
        None => crate::containers::get_container_runtime(),
    };
    let command_argv = argv
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let exec_argv = runtime.build_exec_argv(container_name, "", &command_argv);
    let mut command = std::process::Command::new(&exec_argv[0]);
    command.args(&exec_argv[1..]);
    command
}

pub(super) const CONTAINER_BREADCRUMB_SCRIPT: &str = r#"TERM_DIR=$1
LAUNCH_MARKER=$2
EXPECTED_LAUNCH=$3
ACTIVE_ROOT=$4
MANAGED_ROOT=$5
STORE_KIND=$6
EXPECTED_FINGERPRINT=$7
[ -d "$TERM_DIR" ] && [ ! -L "$TERM_DIR" ] || exit 0
[ -f "$LAUNCH_MARKER" ] && [ ! -L "$LAUNCH_MARKER" ] || exit 0
marker_bytes=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | wc -c) || exit 0
[ "$marker_bytes" -le 17408 ] || exit 0
marker_lines=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | wc -l) || exit 0
terminal=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '1p')
marker_launch=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '2p')
marker_pending=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '3p')
marker_fingerprint=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '4p')
case "$terminal" in ''|.|..|*[!A-Za-z0-9._-]*) exit 0 ;; esac
[ -n "$EXPECTED_LAUNCH" ] && [ "$marker_launch" = "$EXPECTED_LAUNCH" ] || exit 0
[ -n "$EXPECTED_FINGERPRINT" ] && [ "$marker_fingerprint" = "$EXPECTED_FINGERPRINT" ] || exit 0
[ "$(( marker_lines + 0 ))" -eq 4 ] || exit 0
[ -n "$marker_pending" ] || exit 0
f="$TERM_DIR/$terminal"
[ -f "$f" ] && [ ! -L "$f" ] || exit 0
# Post-launch authorship: the marker is written just before exec (host: mtime guard).
[ "$f" -nt "$LAUNCH_MARKER" ] || exit 0
breadcrumb_bytes=$(head -c 16385 "$f" 2>/dev/null | wc -c) || exit 0
[ "$breadcrumb_bytes" -le 16384 ] || exit 0
# A CRLF breadcrumb keeps its CR here and fails closed below.
cwd=$(head -c 16385 "$f" 2>/dev/null | sed -n '1p')
session_path=$(head -c 16385 "$f" 2>/dev/null | sed -n '2p')
extra_1=$(head -c 16385 "$f" 2>/dev/null | sed -n '3p')
extra_2=$(head -c 16385 "$f" 2>/dev/null | sed -n '4p')
breadcrumb_lines=$(head -c 16385 "$f" 2>/dev/null | sed -n '$=') || exit 0
case "$breadcrumb_lines" in 2|3|4) ;; *) exit 0 ;; esac
[ -n "$cwd" ] && [ -n "$session_path" ] || exit 0
marker=
cwdstat_seen=
validate_extra() {
  case "$1" in
    fresh) [ -z "$marker" ] || exit 0; marker=fresh ;;
    'cwdstat '*)
      [ -z "$cwdstat_seen" ] || exit 0
      cwdstat_values=${1#cwdstat }
      cwdstat_dev=${cwdstat_values%% *}
      cwdstat_ino=${cwdstat_values#* }
      [ "$cwdstat_ino" != "$cwdstat_values" ] && [ -n "$cwdstat_dev" ] && [ -n "$cwdstat_ino" ] || exit 0
      case "$cwdstat_dev$cwdstat_ino" in *[!0-9]*) exit 0 ;; esac
      cwdstat_seen=1
      ;;
    *) exit 0 ;;
  esac
}
[ "$breadcrumb_lines" -lt 3 ] || validate_extra "$extra_1"
[ "$breadcrumb_lines" -lt 4 ] || validate_extra "$extra_2"
[ "$session_path" != "$marker_pending" ] || exit 0
full_path=$session_path
case "$full_path" in /*) ;; *) full_path="$cwd/$full_path" ;; esac
exists=0
header=
if [ -f "$full_path" ] && [ ! -L "$full_path" ]; then
  canonical_full=$(realpath "$full_path" 2>/dev/null) || exit 0
  canonical_active=$(realpath "$ACTIVE_ROOT" 2>/dev/null) || canonical_active=
  canonical_managed=$(realpath "$MANAGED_ROOT" 2>/dev/null) || canonical_managed=
  valid_store=0
  # Store-shape parity: mirrors Rust has_store_shape (managed=2, custom=1).
  if [ -n "$canonical_active" ]; then
    case "$canonical_full" in
      "$canonical_active"/*)
        relative=${canonical_full#"$canonical_active"/}
        if [ "$STORE_KIND" = custom ]; then
          case "$relative" in */*) ;; *) valid_store=1 ;; esac
        else
          case "$relative" in */*/*) ;; */*) valid_store=1 ;; esac
        fi
        ;;
    esac
  fi
  if [ -n "$canonical_managed" ]; then
    case "$canonical_full" in
      "$canonical_managed"/*)
        relative=${canonical_full#"$canonical_managed"/}
        case "$relative" in */*/*) ;; */*) valid_store=1 ;; esac
        ;;
    esac
  fi
  [ "$valid_store" = 1 ] || exit 0
  exists=1
  # Anchored so a quoted "type":"session" inside another record cannot match
  # (OMP 17.2.10 writes compact, type-first headers). Byte and line windows
  # mirror PI_HEADER_SCAN_BYTES and PI_HEADER_SCAN_LINES.
  header=$(head -c 65536 "$canonical_full" | head -n 8 | grep -m1 '^{"type":"session"')
fi
marker_bytes_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | wc -c) || exit 0
[ "$marker_bytes_after" -le 17408 ] || exit 0
terminal_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '1p')
launch_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '2p')
pending_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '3p')
fingerprint_after=$(head -c 17409 "$LAUNCH_MARKER" 2>/dev/null | sed -n '4p')
[ "$terminal_after" = "$terminal" ] && [ "$launch_after" = "$marker_launch" ] \
  && [ "$pending_after" = "$marker_pending" ] \
  && [ "$fingerprint_after" = "$marker_fingerprint" ] || exit 0
printf '===OMP===\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n===END===\n' \
  "$terminal" "$marker_launch" "$cwd" "$session_path" "$marker" "$exists" "$header""#;

pub(super) fn select_omp_session_in_container(
    stdout: &[u8],
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = std::str::from_utf8(stdout).context("OMP container capture is not UTF-8")?;
    let body = text
        .strip_prefix("===OMP===\n")
        .and_then(|text| text.strip_suffix("\n===END===\n"))
        .context("No valid OMP terminal breadcrumb found in container")?;
    let mut fields = body.split('\n');
    let (
        Some(terminal_id),
        Some(marker_launch),
        Some(cwd),
        Some(path),
        Some(marker),
        Some(exists),
        Some(header),
    ) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    )
    else {
        anyhow::bail!("Malformed OMP terminal breadcrumb response");
    };
    if fields.next().is_some()
        || !valid_omp_terminal_id(terminal_id)
        || marker_launch != metadata.launch_id.as_str()
        || !matches!(marker, "" | "fresh")
        || !matches!(exists, "0" | "1")
    {
        anyhow::bail!("OMP terminal breadcrumb response has invalid identity fields");
    }
    let breadcrumb = Breadcrumb {
        cwd,
        session_path: path,
        fresh: marker == "fresh",
    };
    let parsed_header = if exists == "1" {
        Some(
            super::super::pi::parse_pi_header_json(header)
                .context("OMP container session JSONL has no valid session header")?,
        )
    } else {
        None
    };
    let session_path = lexical_store_session_path(&metadata.layout, &breadcrumb)?;
    let id = validate_breadcrumb(breadcrumb, &session_path, parsed_header, exclusion)?;
    Ok(id)
}

pub(super) fn capture_omp_session_in_container(
    container_name: &str,
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    launch_marker: &str,
) -> Result<String> {
    validate_omp_capture_metadata(metadata)?;
    let terminals = metadata
        .layout
        .terminal_sessions
        .to_str()
        .context("OMP container terminal path is not UTF-8")?;
    if launch_marker.is_empty() {
        anyhow::bail!("OMP sandbox launch marker is unavailable");
    }
    let active = metadata
        .layout
        .sessions
        .to_str()
        .context("OMP container session path is not UTF-8")?;
    let managed = metadata
        .layout
        .managed_sessions
        .to_str()
        .context("OMP container managed session path is not UTF-8")?;
    let kind = match metadata.layout.kind {
        OmpStoreKind::Managed => "managed",
        OmpStoreKind::Custom => "custom",
    };
    let command = container_exec_command(
        container_name,
        metadata.container_runtime,
        &[
            "sh",
            "-c",
            CONTAINER_BREADCRUMB_SCRIPT,
            "aoe-omp-capture",
            terminals,
            launch_marker,
            &metadata.launch_id,
            active,
            managed,
            kind,
            &metadata.routing_fingerprint,
        ],
    );
    let output = super::super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (OMP breadcrumb capture)",
        MAX_CONTAINER_CAPTURE_BYTES,
    )?;
    select_omp_session_in_container(&output, metadata, exclusion)
}

/// One-shot sandbox capture bound exclusively by the launch marker.
pub(crate) fn try_capture_omp_session_id_in_container(
    container_name: &str,
    metadata: &OmpCaptureMetadata,
    exclusion: &HashSet<String>,
    launch_marker: Option<&str>,
) -> Result<String> {
    capture_omp_session_in_container(
        container_name,
        metadata,
        exclusion,
        launch_marker.context("OMP sandbox launch marker is unavailable")?,
    )
}

/// Sandbox poller. Every tick reloads the tmux generation from the pane name
/// resolved by the outer poller, then the marker selects the one and only
/// terminal breadcrumb that this launch may own.
pub(crate) fn omp_poll_fn_sandboxed(
    container_name: String,
    instance_id: String,
    launch_marker: Option<String>,
    extra_excludes: HashSet<String>,
) -> impl Fn(&str) -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move |tmux_session_name| {
        let metadata = load_omp_capture_metadata(tmux_session_name)
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "OMP container poll metadata refresh failed: {}", error)
            })
            .ok()?;
        let marker = launch_marker.as_deref()?;
        let exclusion = super::super::compose_exclusion(&instance_id, &extra_excludes);
        let captured =
            capture_omp_session_in_container(&container_name, &metadata, &exclusion, marker)
                .map_err(|error| {
                    tracing::debug!(target: "session.capture", "OMP container poll capture failed: {}", error)
                })
                .ok()?;
        let refreshed = load_omp_capture_metadata(tmux_session_name).ok()?;
        if refreshed != metadata {
            return None;
        }
        super::super::validated_session_id(captured).map(|sid| metadata.session_observation(sid))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn container_probe_command_uses_selected_runtime() {
        use crate::session::config::ContainerRuntimeName;

        let cases = [
            (ContainerRuntimeName::Docker, "docker"),
            (ContainerRuntimeName::Podman, "podman"),
            (ContainerRuntimeName::AppleContainer, "container"),
        ];
        for (runtime, expected_binary) in cases {
            let command = container_exec_command("aoe-test", Some(runtime), &["env"]);
            assert_eq!(command.get_program(), expected_binary, "{runtime:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn container_script_bounds_inputs_and_reads_only_the_marker_terminal() {
        assert!(CONTAINER_BREADCRUMB_SCRIPT.contains(&format!(
            "head -n {} | grep",
            super::super::super::pi::PI_HEADER_SCAN_LINES
        )));
        let tmp = tempfile::tempdir().unwrap();
        let meta = metadata(tmp.path(), 100_000);
        let cwd = "/workspace/project";
        let session = session_in(&meta.layout.sessions.join("bucket"), ID);
        write_session(&session, ID, cwd, "");
        std::fs::create_dir_all(&meta.layout.terminal_sessions).unwrap();
        let marker = tmp.path().join("launch-marker");
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "/pending")).unwrap();
        let breadcrumb = meta.layout.terminal_sessions.join("pts-9");
        std::fs::write(&breadcrumb, format!("{cwd}\n{}\n", session.display())).unwrap();
        // Far ahead of every marker rewrite below, to satisfy the `-nt` guard.
        set_mtime_ms(&breadcrumb, 4_000_000_000_000);
        std::fs::write(
            meta.layout.terminal_sessions.join("pts-decoy"),
            format!("{cwd}\n{}\nfresh\n", session.display()),
        )
        .unwrap();

        let output = run_container_script(&meta, &marker);
        assert_eq!(
            select_omp_session_in_container(&output, &meta, &HashSet::new()).unwrap(),
            ID
        );
        for (extras, accepted) in [
            ("", true),
            ("fresh\n", true),
            ("cwdstat 12 34\n", true),
            ("fresh\ncwdstat 12 34\n", true),
            ("cwdstat 12 34\nfresh\n", true),
            ("unknown\n", false),
            ("cwdstat 12\n", false),
            ("cwdstat 12 34 56\n", false),
            ("fresh\nfresh\n", false),
            ("cwdstat 12 34\ncwdstat 12 34\n", false),
        ] {
            std::fs::write(
                &breadcrumb,
                format!("{cwd}\n{}\n{extras}", session.display()),
            )
            .unwrap();
            set_mtime_ms(&breadcrumb, 4_000_000_000_000);
            let output = run_container_script(&meta, &marker);
            assert_eq!(!output.is_empty(), accepted, "{extras:?}");
        }
        std::fs::write(&breadcrumb, format!("{cwd}\n{}\n", session.display())).unwrap();
        set_mtime_ms(&breadcrumb, 4_000_000_000_000);
        let output = run_container_script(&meta, &marker);
        assert_eq!(
            select_omp_session_in_container(&output, &meta, &HashSet::new()).unwrap(),
            ID
        );
        for pending in ["", &*session.to_string_lossy()] {
            std::fs::write(&marker, launch_marker(&meta, "pts-9", pending)).unwrap();
            assert!(
                run_container_script(&meta, &marker).is_empty(),
                "{pending:?}"
            );
        }
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "/pending")).unwrap();

        std::fs::write(&breadcrumb, vec![b'x'; MAX_BREADCRUMB_BYTES + 1]).unwrap();
        set_mtime_ms(&breadcrumb, 4_000_000_000_000);
        assert!(run_container_script(&meta, &marker).is_empty());

        std::fs::write(&breadcrumb, format!("{cwd}\n{}\n", session.display())).unwrap();
        std::fs::write(&marker, vec![b'x'; MAX_LAUNCH_MARKER_BYTES + 1]).unwrap();
        assert!(run_container_script(&meta, &marker).is_empty());
        std::fs::write(&marker, launch_marker(&meta, "pts-9", "/pending")).unwrap();

        let marker_link = tmp.path().join("launch-marker-link");
        std::os::unix::fs::symlink(&marker, &marker_link).unwrap();
        assert!(run_container_script(&meta, &marker_link).is_empty());
    }

    /// A marked generation with a session at `dirs` under the store, a
    /// breadcrumb at `crumb_ms` and the marker at 100s (whole seconds for BSD `-nt`).
    #[cfg(unix)]
    fn container_fixture(
        tmp: &Path,
        dirs: &[&str],
        header_extra: &str,
        crumb_ms: u64,
    ) -> OmpCaptureMetadata {
        let mut meta = metadata(tmp, 100_000);
        let session = session_in(
            &dirs
                .iter()
                .fold(meta.layout.sessions.clone(), |p, d| p.join(d)),
            ID,
        );
        write_session(&session, ID, "/workspace/project", header_extra);
        let breadcrumb = write_breadcrumb(
            &meta,
            "pts-7",
            Path::new("/workspace/project"),
            &session,
            false,
        );
        set_mtime_ms(&breadcrumb, crumb_ms);
        let marker = tmp.join("launch-marker");
        std::fs::write(&marker, launch_marker(&meta, "pts-7", "/pending")).unwrap();
        set_mtime_ms(&marker, 100_000);
        meta.launch_marker = marker.to_string_lossy().into_owned();
        meta
    }

    #[cfg(unix)]
    #[test]
    fn host_and_container_store_shape_verdicts_match() {
        let cases: [(&str, &[&str], bool); 3] = [
            ("in-store", &["bucket"], true),
            ("too-shallow", &[], false),
            ("too-deep", &["a", "b"], false),
        ];
        for (label, dirs, accepted) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let meta = container_fixture(tmp.path(), dirs, "", 101_000);
            let host = capture_omp_session_id_from_terminal(&meta, &HashSet::new(), "pts-7");
            let output = run_container_script(&meta, Path::new(&meta.launch_marker));
            let container = select_omp_session_in_container(&output, &meta, &HashSet::new());
            assert_eq!(host.ok().as_deref(), accepted.then_some(ID), "{label}");
            assert_eq!(container.ok().as_deref(), accepted.then_some(ID), "{label}");
        }
    }

    /// Container freshness parity (#3230), with a header past the probe cap
    /// that must still fit the transport cap.
    #[cfg(unix)]
    #[test]
    fn container_capture_requires_post_launch_breadcrumb_and_carries_large_header() {
        let pad = "x".repeat(32 * 1024);
        assert!(pad.len() > MAX_CONTAINER_PROBE_BYTES);
        let tmp = tempfile::tempdir().unwrap();
        let meta = container_fixture(
            tmp.path(),
            &["bucket"],
            &format!(",\"pad\":\"{pad}\""),
            1_000,
        );
        let marker = PathBuf::from(&meta.launch_marker);
        let breadcrumb = meta.layout.terminal_sessions.join("pts-7");
        assert!(select_omp_session_in_container(
            &run_container_script(&meta, &marker),
            &meta,
            &HashSet::new()
        )
        .is_err());
        set_mtime_ms(&breadcrumb, 200_000);
        assert_eq!(
            select_omp_session_in_container(
                &run_container_script(&meta, &marker),
                &meta,
                &HashSet::new()
            )
            .unwrap(),
            ID
        );
    }

    #[test]
    fn sandbox_record_selection() {
        let meta = metadata(Path::new("/root/.omp/agent"), 100_000);
        let other = "019fc9df-34e1-7000-949e-43ecb1b5c08d";
        let sessions = meta.layout.sessions.display();
        let unmaterialized = |terminal: &str, launch_id: &str, id: &str| {
            format!(
                "===OMP===\n{terminal}\n{launch_id}\n/workspace/project\n{sessions}/bucket/2026-01-01T00-00-00-000Z_{id}.jsonl\nfresh\n0\n\n===END===\n"
            )
        };
        let materialized = |id: &str| {
            format!(
                "===OMP===\npts-9\n{}\n/workspace/project\n{sessions}/bucket/2025-01-01T00-00-00-000Z_{id}.jsonl\n\n1\n{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/workspace/project\"}}\n===END===\n",
                meta.launch_id
            )
        };
        let select = |output: &str| {
            select_omp_session_in_container(output.as_bytes(), &meta, &HashSet::new())
        };
        assert!(select(&unmaterialized("pts-9", "launch-b", ID)).is_err());
        // No mtime proof is needed once the marker selected the breadcrumb, and a
        // later resume of a historical session is accepted.
        assert_eq!(select(&materialized(ID)).unwrap(), ID);
        assert_eq!(select(&materialized(other)).unwrap(), other);
        let two_records = materialized(ID) + &unmaterialized("pts-10", &meta.launch_id, other);
        assert!(select(&two_records).is_err(), "exactly one terminal");
    }
}
