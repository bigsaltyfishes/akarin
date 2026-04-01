use atomig::Atom;
use bitflags::bitflags;

use crate::registry::Index;

bitflags! {
    /// Capabilities define the permissions associated with an object handle.
    ///
    /// Each capability corresponds to a specific permission that determines
    /// what operations can be performed on the object through the handle.
    /// Multiple capabilities can be combined using bitwise OR operations.
    ///
    /// # Capability Groups
    ///
    /// `READ`/`WRITE`/`EXECUTE`/`ADMIN`/`AGENT` are generic kernel-level
    /// capabilities. They define only default semantics for the object system:
    /// metadata access, metadata mutation, control-plane invocation, elevated
    /// management, and delegated management.
    ///
    /// Object-specific interface capabilities (`interface_caps`) are interpreted
    /// by each control plane implementation and can further restrict or expand
    /// behavior inside that object. Therefore, "full control" for `ADMIN`/`AGENT`
    /// is limited to these generic defaults and does not override object-defined
    /// interface rules.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Capability: u32 {
        /// [`CLONE`](Capability::CLONE) capability allows the handle to be
        /// duplicated.
        const CLONE = 1 << 0;
        /// [`SEND`](Capability::SEND) capability allows the handle to be
        /// transferred between processes.
        const SEND = 1 << 1;
        /// Read generic object metadata (name, status, children, mask).
        ///
        /// This is one default capability in the kernel object model. It does
        /// not imply object payload access unless the object's own control plane
        /// exposes such behavior through interface-specific rules.
        const READ = 1 << 2;
        /// Mutate generic object metadata.
        ///
        /// Typical default operations include adding/removing namespace
        /// children and mutating capability masks.
        const WRITE = 1 << 3;
        /// Invoke control-plane operations on the object payload.
        ///
        /// The concrete behavior is object-defined and additionally constrained
        /// by interface capabilities.
        const EXECUTE = 1 << 4;
        /// Generic admin privilege bit.
        ///
        /// In kernel default semantics, `ADMIN` authorizes lifecycle-level
        /// actions (for example privileged removal and privileged derivation).
        /// Object-specific interface behavior remains defined by the object's
        /// control plane.
        const ADMIN = 1 << 5;
        /// Generic delegated-management privilege bit.
        ///
        /// In kernel default semantics, `AGENT` can manage and forward within
        /// delegated scope, but does not imply full lifecycle ownership.
        /// Object-specific interface behavior remains object-defined.
        const AGENT = 1 << 6;

        /// Default owner-level capability group.
        ///
        /// Includes generic `ADMIN/AGENT/READ/WRITE/EXECUTE` defaults.
        const ADMIN_GRP = Self::ADMIN.bits()
            | Self::READ.bits()
            | Self::WRITE.bits()
            | Self::EXECUTE.bits()
            | Self::AGENT.bits();
        /// Default delegated-management capability group.
        ///
        /// Includes generic `AGENT/SEND/READ/WRITE/EXECUTE` defaults.
        const AGENT_GRP = Self::AGENT.bits()
            | Self::SEND.bits()
            | Self::READ.bits()
            | Self::WRITE.bits()
            | Self::EXECUTE.bits();

        /// [`TRUSTED`](Capability::TRUSTED) capability means the handle has all
        /// capabilities without restriction. This is typically reserved for
        /// system-level processes or trusted components. It should not be
        /// granted to untrusted processes.
        const TRUSTED = u32::MAX;
    }
}

impl Atom for Capability {
    type Repr = u32;

    fn pack(self) -> Self::Repr {
        self.bits()
    }

    fn unpack(src: Self::Repr) -> Self {
        Self::from_bits_truncate(src)
    }
}

/// Token associated with an object handle, defining the permissions and
/// capabilities that the handle grants to its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// The index of the object associated with the handle.
    pub(crate) id: Option<Index>,
    /// The capabilities associated with the handle.
    pub(crate) capabilities: Capability,
    /// Interface capabilities that may be used for fine-grained access control.
    ///
    /// Only handles with [`EXECUTE`](Capability::EXECUTE) capability may carry
    /// a non-zero value in this field. The interpretation of this bitset is
    /// object-defined.
    pub(crate) interface_caps: u32,
}

impl Token {
    /// Create a new [`Token`] with the specified ID and capabilities.
    pub(crate) const fn new(
        id: Option<Index>,
        capabilities: Capability,
        interface_caps: u32,
    ) -> Self {
        if interface_caps != u32::MAX
            && (capabilities.contains(Capability::ADMIN)
                || capabilities.contains(Capability::AGENT))
        {
            panic!(
                "ADMIN or AGENT handles should have interface_caps set to u32::MAX to indicate no \
                 restriction, otherwise it may cause confusion when deriving lower-privilege \
                 handles"
            );
        }

        Self {
            id,
            capabilities,
            interface_caps,
        }
    }

    /// Check if the [`Token`] contain the specified capability.
    ///
    /// # Arguments
    ///
    /// * `cap` - The capability to check for.
    ///
    /// # Returns
    ///
    /// * `true` if the token contain the specified capability, `false`
    ///   otherwise.
    pub fn contains(&self, cap: Capability) -> bool {
        self.capabilities.contains(cap)
    }

    /// Get the object-specific capabilities of the token.
    ///
    /// This is only meaningful for handles with
    /// [`EXECUTE`](Capability::EXECUTE) capability. The interpretation of
    /// this field is defined by the object implementation and can be used
    /// for fine-grained access control or to convey additional information
    /// about the handle's permissions.
    pub fn interface_caps(&self) -> u32 {
        self.interface_caps
    }

    /// Get the generic capabilities of the token.
    pub fn capabilities(&self) -> Capability {
        self.capabilities
    }

    /// Downgrade the [`Token`] by removing the specified capabilities.
    ///
    /// # Arguments
    ///
    /// * `caps` - The capabilities to remove from the token.
    /// * `interface_caps` - The interface capabilities to remove from the
    ///   token.
    pub fn downgrade(&mut self, caps: Capability, interface_caps: u32) {
        self.capabilities.remove(caps);
        self.interface_caps &= !interface_caps;
    }
}

impl AsRef<Self> for Token {
    fn as_ref(&self) -> &Token {
        self
    }
}
