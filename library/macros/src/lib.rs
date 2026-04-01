#![no_std]

pub use libakarin_macros_derive::abstraction;

#[macro_export]
macro_rules! cpu_local {
    (@item $(#[$meta:meta])* $vis:vis static $(mut)? $name:ident : $ty:ty = $init:expr) => {
        $(#[$meta])*
        #[unsafe(link_section = "__DATA,__percpu")]
        $vis static $name: ::libakarin_machine_core::cpu::PerCpuVar<$ty> =
            ::libakarin_machine_core::cpu::PerCpuVar::new($init);
    };
    ($(#[$meta:meta])* $vis:vis static $(mut)? $name:ident : $ty:ty = $init:expr $(;)?) => {
        $crate::cpu_local!(@item $(#[$meta])* $vis static $name: $ty = $init);
    };
    ($(
        $(#[$meta:meta])* $vis:vis static $(mut)? $name:ident : $ty:ty = $init:expr;
    )+) => {
        $(
            $crate::cpu_local!(@item $(#[$meta])* $vis static $name: $ty = $init);
        )+
    };
}

#[macro_export]
macro_rules! align_down {
    ($value:expr, $align:expr) => {
        (($value) & !(($align) - 1))
    };
}

#[macro_export]
macro_rules! align_up {
    ($value:expr, $align:expr) => {
        (($value) + (($align) - 1)) & !(($align) - 1)
    };
}

#[macro_export]
macro_rules! prev_power_of_two {
    ($type:ty, $value:expr) => {
        ((1 as $type) << (<$type>::BITS as $type - ($value).leading_zeros() as $type - 1))
    };
}
