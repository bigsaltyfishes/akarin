#[inline]
pub(crate) const fn prev_power_of_two(num: usize) -> usize {
    1 << (usize::BITS as usize - num.leading_zeros() as usize - 1)
}

#[inline]
pub const fn align_up(page_size: usize, addr: usize) -> usize {
    align_down(page_size, addr + page_size - 1)
}

#[inline]
pub const fn align_down(page_size: usize, addr: usize) -> usize {
    addr & !(page_size - 1)
}
