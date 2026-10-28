//! Who a request acts as (ADR 0002). `auth_middleware` stamps one per authenticated request.

/// `Proxy` joins when #3839's configured proxy ingress exists to construct it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    /// The peer on the daemon's Unix socket, with its uid from the kernel.
    LocalPeer { uid: u32 },
    /// A signed-in browser login.
    Device,
    /// A caller holding the access token, or any caller under `--auth none`.
    TokenHolder,
}

/// What a Principal may do beyond what every authenticated caller can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Mutate without re-entering the passphrase.
    SkipElevation,
    /// Read hook output of an in-flight create.
    CreationProgress,
    /// Drive native interaction on a session.
    NativeInteraction,
    /// See hook failure detail in create errors.
    CreateHookDetail,
    /// See owner diagnostics and the serve instance id.
    OwnerDiagnostics,
}

/// The one decision point for Principal-dependent authority. An absent Principal holds none.
pub fn authorize(principal: Option<&Principal>, capability: Capability) -> bool {
    match (principal, capability) {
        (
            Some(Principal::LocalPeer { .. }),
            Capability::SkipElevation
            | Capability::CreationProgress
            | Capability::NativeInteraction
            | Capability::CreateHookDetail
            | Capability::OwnerDiagnostics,
        ) => true,
        (Some(Principal::Device | Principal::TokenHolder) | None, _) => false,
    }
}

impl Principal {
    pub fn is_local_peer(&self) -> bool {
        matches!(self, Self::LocalPeer { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_local_peer_holds_owner_capabilities() {
        let capabilities = [
            Capability::SkipElevation,
            Capability::CreationProgress,
            Capability::NativeInteraction,
            Capability::CreateHookDetail,
            Capability::OwnerDiagnostics,
        ];
        for (principal, expected) in [
            (None, false),
            (Some(Principal::Device), false),
            (Some(Principal::TokenHolder), false),
            (Some(Principal::LocalPeer { uid: 1000 }), true),
        ] {
            for capability in capabilities {
                assert_eq!(
                    authorize(principal.as_ref(), capability),
                    expected,
                    "{principal:?} {capability:?}"
                );
            }
        }
    }
}
