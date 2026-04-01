use alloc::fmt;
use core::{
    fmt::{Formatter, LowerHex},
    ops::{Add, AddAssign, Shl, ShlAssign, Shr, ShrAssign, Sub, SubAssign},
};

/// A virtual address wrapper with arithmetic operations.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct VirtAddr(usize);

impl VirtAddr {
    #[inline]
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    #[inline]
    pub fn as_usize(&self) -> usize {
        self.0
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.0 as *const u8
    }

    #[inline]
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.0 as *mut u8
    }
}

impl Add<usize> for VirtAddr {
    type Output = VirtAddr;

    #[inline]
    fn add(self, rhs: usize) -> Self::Output {
        VirtAddr(self.0 + rhs)
    }
}

impl Sub<usize> for VirtAddr {
    type Output = VirtAddr;

    #[inline]
    fn sub(self, rhs: usize) -> Self::Output {
        VirtAddr(self.0 - rhs)
    }
}

impl Add<VirtAddr> for usize {
    type Output = VirtAddr;

    #[inline]
    fn add(self, rhs: VirtAddr) -> Self::Output {
        VirtAddr(self + rhs.0)
    }
}

impl Sub<VirtAddr> for usize {
    type Output = VirtAddr;

    #[inline]
    fn sub(self, rhs: VirtAddr) -> Self::Output {
        VirtAddr(self - rhs.0)
    }
}

impl AddAssign<usize> for VirtAddr {
    #[inline]
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl SubAssign<usize> for VirtAddr {
    #[inline]
    fn sub_assign(&mut self, rhs: usize) {
        self.0 -= rhs;
    }
}

impl AddAssign<VirtAddr> for usize {
    #[inline]
    fn add_assign(&mut self, rhs: VirtAddr) {
        *self += rhs.0;
    }
}

impl SubAssign<VirtAddr> for usize {
    #[inline]
    fn sub_assign(&mut self, rhs: VirtAddr) {
        *self -= rhs.0;
    }
}

impl Shl<usize> for VirtAddr {
    type Output = VirtAddr;

    #[inline]
    fn shl(self, rhs: usize) -> Self::Output {
        VirtAddr(self.0 << rhs)
    }
}

impl ShlAssign<usize> for VirtAddr {
    #[inline]
    fn shl_assign(&mut self, rhs: usize) {
        self.0 <<= rhs;
    }
}

impl Shr<usize> for VirtAddr {
    type Output = VirtAddr;

    #[inline]
    fn shr(self, rhs: usize) -> Self::Output {
        VirtAddr(self.0 >> rhs)
    }
}

impl ShrAssign<usize> for VirtAddr {
    #[inline]
    fn shr_assign(&mut self, rhs: usize) {
        self.0 >>= rhs;
    }
}

/// A physical address wrapper.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct PhysAddr(usize);

impl PhysAddr {
    #[inline]
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    #[inline]
    pub fn as_usize(&self) -> usize {
        self.0
    }
}

impl Add<usize> for PhysAddr {
    type Output = PhysAddr;

    #[inline]
    fn add(self, rhs: usize) -> Self::Output {
        PhysAddr(self.0 + rhs)
    }
}

impl Sub<usize> for PhysAddr {
    type Output = PhysAddr;

    #[inline]
    fn sub(self, rhs: usize) -> Self::Output {
        PhysAddr(self.0 - rhs)
    }
}

impl Add<PhysAddr> for usize {
    type Output = PhysAddr;

    #[inline]
    fn add(self, rhs: PhysAddr) -> Self::Output {
        PhysAddr(self + rhs.0)
    }
}

impl Sub<PhysAddr> for usize {
    type Output = PhysAddr;

    #[inline]
    fn sub(self, rhs: PhysAddr) -> Self::Output {
        PhysAddr(self - rhs.0)
    }
}

impl AddAssign<usize> for PhysAddr {
    #[inline]
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl SubAssign<usize> for PhysAddr {
    #[inline]
    fn sub_assign(&mut self, rhs: usize) {
        self.0 -= rhs;
    }
}

impl AddAssign<PhysAddr> for usize {
    #[inline]
    fn add_assign(&mut self, rhs: PhysAddr) {
        *self += rhs.0;
    }
}

impl SubAssign<PhysAddr> for usize {
    #[inline]
    fn sub_assign(&mut self, rhs: PhysAddr) {
        *self -= rhs.0;
    }
}

impl Shl<usize> for PhysAddr {
    type Output = PhysAddr;

    #[inline]
    fn shl(self, rhs: usize) -> Self::Output {
        PhysAddr(self.0 << rhs)
    }
}

impl ShlAssign<usize> for PhysAddr {
    #[inline]
    fn shl_assign(&mut self, rhs: usize) {
        self.0 <<= rhs;
    }
}

impl Shr<usize> for PhysAddr {
    type Output = PhysAddr;

    #[inline]
    fn shr(self, rhs: usize) -> Self::Output {
        PhysAddr(self.0 >> rhs)
    }
}

impl ShrAssign<usize> for PhysAddr {
    #[inline]
    fn shr_assign(&mut self, rhs: usize) {
        self.0 >>= rhs;
    }
}

impl LowerHex for PhysAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl LowerHex for VirtAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}
