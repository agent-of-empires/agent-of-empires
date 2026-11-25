//! The automatic sandbox image: the `aoe-sandbox` tag released with this build.

use super::error::DockerError;
use super::image_pull::PullFailureReason;
use std::sync::LazyLock;

const SANDBOX_IMAGE_REPOSITORY: &str = "ghcr.io/agent-of-empires/aoe-sandbox";

struct AutomaticImages {
    exact: String,
    /// The floating `<major>.<minor>` tag, tried when the exact one cannot be
    /// pulled.
    minor: Option<String>,
}

static AUTOMATIC: LazyLock<AutomaticImages> =
    LazyLock::new(|| images_for_version(env!("CARGO_PKG_VERSION")));

/// The image an unset `sandbox.default_image` resolves to. Every release tag
/// publishes `aoe-sandbox:<x.y.z>`, so the image never lags the binary.
pub fn automatic_sandbox_image() -> &'static str {
    &AUTOMATIC.exact
}

/// The automatic image without its registry path, for compact displays.
pub fn automatic_sandbox_image_short() -> &'static str {
    let image = automatic_sandbox_image();
    image.rsplit_once('/').map_or(image, |(_, name)| name)
}

fn images_for_version(version: &str) -> AutomaticImages {
    let Some(([major, minor, patch], prerelease)) = release_parts(version) else {
        tracing::warn!(
            target: "containers.image",
            %version,
            "cannot derive a release tag from the build version; using the latest sandbox image"
        );
        return AutomaticImages {
            exact: format!("{SANDBOX_IMAGE_REPOSITORY}:latest"),
            minor: None,
        };
    };
    // A prerelease tag is published as is, and the floating minor tag belongs to the
    // final release, so nothing stands in for it.
    match prerelease {
        // Nightly images are published as `:nightly` only.
        Some(pre) if pre == "nightly" || pre.starts_with("nightly.") => AutomaticImages {
            exact: format!("{SANDBOX_IMAGE_REPOSITORY}:nightly"),
            minor: None,
        },
        Some(pre) => AutomaticImages {
            exact: format!("{SANDBOX_IMAGE_REPOSITORY}:{major}.{minor}.{patch}-{pre}"),
            minor: None,
        },
        None => AutomaticImages {
            exact: format!("{SANDBOX_IMAGE_REPOSITORY}:{major}.{minor}.{patch}"),
            minor: Some(format!("{SANDBOX_IMAGE_REPOSITORY}:{major}.{minor}")),
        },
    }
}

/// `x.y.z` and the prerelease, with build metadata stripped, or `None` when the
/// version is not three numeric components plus an optional well-formed prerelease.
fn release_parts(version: &str) -> Option<([&str; 3], Option<&str>)> {
    let without_build = version.split('+').next()?;
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };
    let parts: [&str; 3] = core.split('.').collect::<Vec<_>>().try_into().ok()?;
    let numeric = parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    let tag_safe = prerelease.is_none_or(|pre| {
        !pre.is_empty()
            && pre
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    (numeric && tag_safe).then_some((parts, prerelease))
}

/// Where images are found and fetched; the container runtime in production.
pub(crate) trait ImageSource {
    fn exists_locally(&self, image: &str) -> bool;
    fn pull(&self, image: &str) -> Result<(), DockerError>;
}

/// Makes `image` available and returns the image to run. Only the automatic
/// image falls back, to its `<major>.<minor>` tag, when its exact tag cannot be
/// pulled: right after a release, before the tag is published, or offline.
/// `automatic` says no `default_image` is configured; one naming the same tag
/// stays as set. An image requested for one session is stored as a bare string,
/// so a request naming this tag with none configured falls back too.
pub(crate) fn ensure_sandbox_image(
    source: &impl ImageSource,
    image: &str,
    automatic: bool,
) -> anyhow::Result<String> {
    let minor = (automatic && image == automatic_sandbox_image())
        .then_some(AUTOMATIC.minor.as_deref())
        .flatten();
    ensure_with_fallback(source, image, minor)
}

/// Neither automatic tag could be pulled. The tags are derived from the AoE version, so
/// unlike the runtime's output they are safe to show.
/// The typed pull error is its source, so it stays in the chain.
#[derive(Debug)]
pub struct AutomaticImagesUnavailable {
    pub exact: String,
    pub minor: String,
    pub source: DockerError,
}

impl std::error::Error for AutomaticImagesUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl std::fmt::Display for AutomaticImagesUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not pull {}, the sandbox image tagged for this AoE version, \
             nor its minor tag {}: not published yet or the registry is unreachable; \
             retry later or set [sandbox] default_image to another image",
            self.exact, self.minor
        )
    }
}

/// The registry refused the tag, timed out or could not be reached; a daemon
/// that is down, a full disk or a cancelled start never falls back.
fn is_unavailable(error: &DockerError) -> bool {
    match error {
        DockerError::ImageNotFound(_) => true,
        DockerError::ImagePullFailed { reason, .. } => *reason != PullFailureReason::DiskSpace,
        _ => false,
    }
}

fn ensure_with_fallback(
    source: &impl ImageSource,
    image: &str,
    minor: Option<&str>,
) -> anyhow::Result<String> {
    let ensure = |image: &str| {
        if source.exists_locally(image) {
            Ok(())
        } else {
            source.pull(image)
        }
    };
    let exact_error = match ensure(image) {
        Ok(()) => return Ok(image.to_string()),
        Err(error) => error,
    };
    let (true, Some(minor)) = (is_unavailable(&exact_error), minor) else {
        return Err(exact_error.into());
    };
    tracing::warn!(
        target: "containers.image",
        exact = %image,
        fallback = %minor,
        error = %exact_error,
        "cannot pull the sandbox image for this AoE version; falling back to its minor tag"
    );
    match ensure(minor) {
        Ok(()) => Ok(minor.to_string()),
        Err(minor_error) if is_unavailable(&minor_error) => {
            tracing::warn!(
                target: "containers.image",
                fallback = %minor,
                error = %minor_error,
                "cannot pull the minor sandbox image either"
            );
            // The typed pull error stays in the chain for the session creation's mapping; the
            // runtime's own output stays out of the context.
            let typed = if matches!(exact_error, DockerError::ImagePullFailed { .. })
                || !matches!(minor_error, DockerError::ImagePullFailed { .. })
            {
                exact_error
            } else {
                minor_error
            };
            Err(AutomaticImagesUnavailable {
                exact: image.to_string(),
                minor: minor.to_string(),
                source: typed,
            }
            .into())
        }
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn image_tags_follow_the_release_version() {
        for (version, exact, minor) in [
            ("1.19.0", "1.19.0", Some("1.19")),
            // A prerelease tag is published as is, and no floating tag stands in for it.
            ("1.19.1-rc.1", "1.19.1-rc.1", None),
            ("1.19.1-rc.1+g57db0613b036", "1.19.1-rc.1", None),
            // Nightly images are published as `:nightly` only.
            ("1.19.1-nightly.20261120", "nightly", None),
            ("1.19.0+g57db0613b036-dirty", "1.19.0", Some("1.19")),
            ("0.10.0", "0.10.0", Some("0.10")),
            ("", "latest", None),
            ("1.19", "latest", None),
            ("1.19.0.1", "latest", None),
            ("v1.19.0", "latest", None),
            ("1..0", "latest", None),
        ] {
            let images = images_for_version(version);
            let repo = "ghcr.io/agent-of-empires/aoe-sandbox";
            assert_eq!(images.exact, format!("{repo}:{exact}"), "{version:?}");
            assert_eq!(
                images.minor,
                minor.map(|minor| format!("{repo}:{minor}")),
                "{version:?}"
            );
        }
    }

    /// Images present locally, images the registry serves, and every call made.
    struct FakeSource {
        local: &'static [&'static str],
        pullable: &'static [&'static str],
        calls: RefCell<Vec<String>>,
    }

    impl ImageSource for FakeSource {
        fn exists_locally(&self, image: &str) -> bool {
            self.calls.borrow_mut().push(format!("local {image}"));
            self.local.contains(&image)
        }

        fn pull(&self, image: &str) -> Result<(), DockerError> {
            self.calls.borrow_mut().push(format!("pull {image}"));
            if self.pullable.contains(&image) {
                Ok(())
            } else {
                Err(DockerError::ImageNotFound(format!(
                    "{image}: manifest unknown"
                )))
            }
        }
    }

    #[test]
    fn only_the_automatic_image_falls_back_to_its_minor_tag() {
        const EXACT: &str = "repo/aoe-sandbox:1.19.0";
        const MINOR: &str = "repo/aoe-sandbox:1.19";
        // (local, pullable, minor tag offered, Ok(used) or Err(expected text), calls)
        type Case = (
            &'static [&'static str],
            &'static [&'static str],
            Option<&'static str>,
            Result<&'static str, &'static str>,
            &'static [&'static str],
        );
        let cases: &[Case] = &[
            (&[EXACT], &[], Some(MINOR), Ok(EXACT), &["local repo/aoe-sandbox:1.19.0"]),
            (
                &[],
                &[EXACT],
                Some(MINOR),
                Ok(EXACT),
                &["local repo/aoe-sandbox:1.19.0", "pull repo/aoe-sandbox:1.19.0"],
            ),
            (
                &[MINOR],
                &[],
                Some(MINOR),
                Ok(MINOR),
                &[
                    "local repo/aoe-sandbox:1.19.0",
                    "pull repo/aoe-sandbox:1.19.0",
                    "local repo/aoe-sandbox:1.19",
                ],
            ),
            (
                &[],
                &[MINOR],
                Some(MINOR),
                Ok(MINOR),
                &[
                    "local repo/aoe-sandbox:1.19.0",
                    "pull repo/aoe-sandbox:1.19.0",
                    "local repo/aoe-sandbox:1.19",
                    "pull repo/aoe-sandbox:1.19",
                ],
            ),
            (
                &[],
                &[],
                Some(MINOR),
                Err("could not pull repo/aoe-sandbox:1.19.0, the sandbox image tagged for this AoE version, \
                     nor its minor tag repo/aoe-sandbox:1.19: not published yet or the registry is unreachable; \
                     retry later or set [sandbox] default_image to another image: \
                     Image not found: repo/aoe-sandbox:1.19.0: manifest unknown"),
                &[
                    "local repo/aoe-sandbox:1.19.0",
                    "pull repo/aoe-sandbox:1.19.0",
                    "local repo/aoe-sandbox:1.19",
                    "pull repo/aoe-sandbox:1.19",
                ],
            ),
            // A custom image is never swapped for another.
            (
                &[MINOR],
                &[MINOR],
                None,
                Err("Image not found: repo/aoe-sandbox:1.19.0: manifest unknown"),
                &["local repo/aoe-sandbox:1.19.0", "pull repo/aoe-sandbox:1.19.0"],
            ),
        ];
        for (local, pullable, minor, expected, calls) in cases {
            let source = FakeSource {
                local,
                pullable,
                calls: RefCell::default(),
            };
            let result = ensure_with_fallback(&source, EXACT, *minor).map_err(|e| format!("{e:#}"));
            match (expected, &result) {
                (Ok(used), Ok(got)) => assert_eq!(got, used),
                (Err(text), Err(got)) => assert!(got.starts_with(text), "{got}"),
                _ => panic!("{local:?} {pullable:?}: {result:?}"),
            }
            assert_eq!(&*source.calls.borrow(), calls, "{local:?} {pullable:?}");
        }
    }

    #[test]
    fn two_failed_pulls_keep_the_typed_pull_error_and_name_both_tags() {
        struct NothingPulls;
        impl ImageSource for NothingPulls {
            fn exists_locally(&self, _image: &str) -> bool {
                false
            }
            fn pull(&self, image: &str) -> Result<(), DockerError> {
                Err(DockerError::ImagePullFailed {
                    runtime: "Docker",
                    image: image.to_string(),
                    detail: "runtime-stderr-detail".to_string(),
                    reason: PullFailureReason::Other,
                })
            }
        }
        let error = ensure_with_fallback(
            &NothingPulls,
            "repo/aoe-sandbox:1.19.0",
            Some("repo/aoe-sandbox:1.19"),
        )
        .unwrap_err();
        assert!(
            matches!(
                error.chain().find_map(|c| c.downcast_ref::<DockerError>()),
                Some(DockerError::ImagePullFailed { .. })
            ),
            "{error:#}"
        );
        let context = error.to_string();
        assert!(context.contains("repo/aoe-sandbox:1.19.0"), "{context}");
        assert!(context.contains("repo/aoe-sandbox:1.19"), "{context}");
        assert!(!context.contains("runtime-stderr-detail"), "{context}");
    }

    #[test]
    fn only_a_missing_image_falls_back() {
        /// The exact tag fails with `error`; the minor tag would pull fine.
        struct ExactFails {
            error: fn() -> DockerError,
            calls: RefCell<Vec<String>>,
        }
        impl ImageSource for ExactFails {
            fn exists_locally(&self, image: &str) -> bool {
                self.calls.borrow_mut().push(format!("local {image}"));
                false
            }
            fn pull(&self, image: &str) -> Result<(), DockerError> {
                self.calls.borrow_mut().push(format!("pull {image}"));
                if image == "a:1.19.0" {
                    Err((self.error)())
                } else {
                    Ok(())
                }
            }
        }
        let stops: &[&str] = &["local a:1.19.0", "pull a:1.19.0"];
        let falls_back: &[&str] = &[
            "local a:1.19.0",
            "pull a:1.19.0",
            "local a:1.19",
            "pull a:1.19",
        ];
        // (exact tag's error, image used or None for the error passed through, calls)
        type Case<'a> = (fn() -> DockerError, Option<&'a str>, &'a [&'a str]);
        let cases: [Case; 6] = [
            // A full disk says nothing about the registry, and a local minor image is no answer.
            (
                || DockerError::ImagePullFailed {
                    runtime: "Docker",
                    image: "a:1.19.0".into(),
                    detail: "no space left on device".into(),
                    reason: PullFailureReason::DiskSpace,
                },
                None,
                stops,
            ),
            (
                || DockerError::ImageNotFound("a:1.19.0: manifest unknown".into()),
                Some("a:1.19"),
                falls_back,
            ),
            (
                || DockerError::ImagePullFailed {
                    runtime: "Docker",
                    image: "a:1.19.0".into(),
                    detail: "manifest unknown".into(),
                    reason: PullFailureReason::Other,
                },
                Some("a:1.19"),
                falls_back,
            ),
            (|| DockerError::Cancelled("pull".into()), None, stops),
            (|| DockerError::DaemonNotRunning, None, stops),
            (|| DockerError::PermissionDenied, None, stops),
        ];
        for (error, expected, calls) in cases {
            let source = ExactFails {
                error,
                calls: RefCell::default(),
            };
            let result = ensure_with_fallback(&source, "a:1.19.0", Some("a:1.19"));
            match (expected, result) {
                (Some(used), Ok(got)) => assert_eq!(got, used),
                (None, Err(got)) => assert_eq!(format!("{got:#}"), error().to_string()),
                (_, got) => panic!("{}: {got:?}", error()),
            }
            assert_eq!(&*source.calls.borrow(), calls, "{}", error());
        }
    }

    #[test]
    fn the_production_seam_offers_the_minor_tag_for_the_automatic_image_only() {
        let source = FakeSource {
            local: &[],
            pullable: &[],
            calls: RefCell::default(),
        };
        let _ = ensure_sandbox_image(&source, "ghcr.io/example/custom:1", true);
        assert_eq!(
            source.calls.borrow().len(),
            2,
            "{:?}",
            source.calls.borrow()
        );

        // An explicit pin of the very same tag is not the automatic image.
        let automatic = automatic_sandbox_image();
        source.calls.borrow_mut().clear();
        let _ = ensure_sandbox_image(&source, automatic, false);
        assert_eq!(
            source.calls.borrow().len(),
            2,
            "{:?}",
            source.calls.borrow()
        );

        // No default configured: a session request naming the automatic tag, stored as a bare
        // string, falls back like the unset default does.
        source.calls.borrow_mut().clear();
        let _ = ensure_sandbox_image(&source, automatic, true);
        let calls = source.calls.borrow();
        match AUTOMATIC.minor.as_deref() {
            Some(minor) => {
                assert_eq!(calls.len(), 4, "{calls:?}");
                assert_eq!(calls[3], format!("pull {minor}"));
            }
            // A prerelease build has no floating tag to fall back to.
            None => assert_eq!(calls.len(), 2, "{calls:?}"),
        }
    }
}
