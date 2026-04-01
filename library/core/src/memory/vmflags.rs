use bitflags::bitflags;
use libakarin_machine_core::memory::paging::{CachePolicy, MMUFlags};

const MMU_LOW_MASK: u32 = u16::MAX as u32;
const VM_ACCESS_MASK: u32 = MMUFlags::READ.bits() as u32
    | MMUFlags::WRITE.bits() as u32
    | MMUFlags::EXECUTE.bits() as u32
    | MMUFlags::USER.bits() as u32
    | MMUFlags::HUGE_PAGE.bits() as u32
    | MMUFlags::DEVICE.bits() as u32
    | MMUFlags::GLOBAL.bits() as u32;

bitflags! {
    /// Unified VM flags used by VMAR mappings and VMO capabilities.
    ///
    /// The low 16 bits mirror [`MMUFlags`]. The high 16 bits are reserved for
    /// VM-specific extensions that are not directly represented in page-table
    /// entries.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VmFlags: u32 {
        const CACHE_1   = MMUFlags::CACHE_1.bits() as u32;
        const CACHE_2   = MMUFlags::CACHE_2.bits() as u32;

        const READ      = MMUFlags::READ.bits() as u32;
        const WRITE     = MMUFlags::WRITE.bits() as u32;
        const EXECUTE   = MMUFlags::EXECUTE.bits() as u32;
        const USER      = MMUFlags::USER.bits() as u32;
        const HUGE_PAGE = MMUFlags::HUGE_PAGE.bits() as u32;
        const DEVICE    = MMUFlags::DEVICE.bits() as u32;
        const GLOBAL    = MMUFlags::GLOBAL.bits() as u32;

        const MAP       = 1 << 16;
        const PIN       = 1 << 17;
        const RESIZABLE = 1 << 18;
        const CONTIGUOUS = 1 << 19;
        const PHYSICAL  = 1 << 20;
    }
}

impl VmFlags {
    /// Return the MMU-visible portion of this flag set.
    pub fn mmu_flags(self) -> MMUFlags {
        MMUFlags::from_bits_truncate((self.bits() & MMU_LOW_MASK) as u16)
    }

    /// Return the VM-extension portion of this flag set.
    pub fn extension_bits(self) -> u16 {
        (self.bits() >> 16) as u16
    }

    /// Return one flag set created from raw MMU flags.
    pub fn from_mmu_flags(flags: MMUFlags) -> Self {
        Self::from_bits_retain(flags.bits() as u32)
    }

    /// Return the cache policy encoded in the low MMU flag bits.
    pub fn cache_policy(self) -> CachePolicy {
        self.mmu_flags().cache_policy()
    }

    /// Return a copy with the cache policy replaced.
    pub fn with_cache_policy(self, policy: CachePolicy) -> Self {
        let mut mmu = self.mmu_flags();
        mmu.set_cache_policy(policy);
        Self::from_bits_retain((self.bits() & !MMU_LOW_MASK) | mmu.bits() as u32)
    }

    /// Return whether this flag set permits the requested MMU-visible access.
    ///
    /// Cache-policy bits and VM-extension bits are intentionally ignored here;
    /// callers use this to validate access permissions, not attribute equality.
    pub fn allows_mmu(self, requested: Self) -> bool {
        (self.bits() & VM_ACCESS_MASK & requested.bits()) == (requested.bits() & VM_ACCESS_MASK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmflags_keep_cache_policy_out_of_access_checks() {
        let allowed = VmFlags::READ | VmFlags::WRITE | VmFlags::USER;
        let requested =
            (VmFlags::READ | VmFlags::USER).with_cache_policy(CachePolicy::WriteCombining);
        assert!(allowed.allows_mmu(requested));
        assert!(!VmFlags::READ.allows_mmu(VmFlags::READ | VmFlags::WRITE));
    }
}
