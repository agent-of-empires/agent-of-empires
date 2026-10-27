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

impl Principal {
    pub fn is_local_peer(&self) -> bool {
        matches!(self, Self::LocalPeer { .. })
    }
}
