use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use crate::sync::IrqSaveGuard;

// Linker symbols for per-CPU data template.
unsafe extern "Rust" {
    #[link_name = "\u{1}section$start$__DATA$__percpu"]
    pub unsafe static PERCPU_TEMPLATE_START: u8;
    #[link_name = "\u{1}section$end$__DATA$__percpu"]
    pub unsafe static PERCPU_TEMPLATE_END: u8;
}

/// The `cpu_local!` macro is used to define CPU-local static variables in the
/// `__DATA,__percpu` section. These variables are used to store data that is
/// specific to each CPU core in a multi-core system. The `cpu_local!` macro
/// provides a convenient way to declare and access these CPU-local variables,
/// ensuring that each CPU core has its own instance of the variable. The
/// variables defined with `cpu_local!` are typically used for storing per-CPU
/// data such as CPU IDs, APIC IDs, or other CPU-specific information that needs
/// to be accessed efficiently without the overhead of synchronization
/// primitives. By placing these variables in a special section, the operating
/// system can easily manage and access them based on the current CPU core,
/// allowing for efficient per-CPU data management in the kernel.
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

/// A trait for per-CPU operations and data management.
///
/// This trait should be implemented to handle CPU-specific tasks,
/// such as managing CPU-local storage, handling CPU-specific interrupts,
/// and performing operations that are specific to each CPU core.
///
/// # Implementation Notes
///
/// `PerCpu` should be initialized and only initialized once during the system
/// startup. Implemtations should combine with a static variable to manage the
/// initialization state, storing the size and address of the per-CPU data
/// template.
pub trait PerCpuTrait {
    fn template() -> &'static [u8] {
        unsafe {
            let start = &PERCPU_TEMPLATE_START as *const u8;
            let end = &PERCPU_TEMPLATE_END as *const u8;
            core::slice::from_raw_parts(start as *const u8, end.offset_from(start) as usize)
        }
    }

    /// Get the current CPU's identifier.
    fn id() -> usize;

    /// Get the total number of CPUs in the system.
    fn count() -> usize;

    /// Initialize all per-CPU data areas.
    ///
    /// # Returns
    ///
    /// The total size of the allocated per-CPU data areas.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it must be called only once during the
    /// system initialization, and it must be called before any other per-CPU
    /// operations are performed. Calling this function multiple times or
    /// calling it after per-CPU data has been accessed can lead to
    /// undefined behavior.
    unsafe fn init() -> usize;

    /// Enable CPU interrupt.
    ///
    /// This function enables interrupts on the current CPU.
    fn enable_interrupt();

    /// Disable CPU interrupt.
    ///
    /// This function disables interrupts on the current CPU.
    fn disable_interrupt();

    /// Check CPU interrupt enabled or not
    fn is_interrupt_enabled() -> bool;

    /// Halt the CPU.
    ///
    /// This function halts the current CPU until the next interrupt arrives.
    fn halt();

    /// Enable CPU interrupt and halt the CPU atomically.
    ///
    /// This function enables interrupts and halts the current CPU atomically,
    /// ensuring that no interrupts are missed between enabling interrupts and
    /// halting the CPU.
    fn idle_until_interrupt();

    /// Returns the number of per-CPU data areas reserved.
    fn area_num() -> usize;

    /// Returns the size of each per-CPU data area.
    fn area_size() -> usize;

    /// Returns the base address of the per-CPU data area for the given CPU ID.
    fn area_base(cpu_id: usize) -> usize;

    /// Reads the architecture-specific register used for CPU-local storage.
    fn reg_read() -> usize;

    /// Writes to the architecture-specific register used for CPU-local storage.
    ///
    /// # Safety
    ///
    /// This function is unsafe because writing to CPU-local storage
    /// registers can lead to undefined behavior if not done correctly.
    unsafe fn reg_write(val: usize);
}

#[derive(Clone, Copy)]
pub struct PerCpuRuntime {
    id: fn() -> usize,
    area_size: fn() -> usize,
    area_base: fn(usize) -> usize,
    area_num: fn() -> usize,
    reg_read: fn() -> usize,
    with_irqsave: fn(&mut dyn FnMut()),
}

impl PerCpuRuntime {
    pub fn setup<G, P>()
    where
        G: IrqSaveGuard,
        P: PerCpuTrait,
    {
        #[inline]
        fn with_irqsave<G>(f: &mut dyn FnMut())
        where
            G: IrqSaveGuard,
        {
            let _guard = G::enter();
            f();
        }

        let runtime = Self {
            id: P::id,
            area_size: P::area_size,
            area_base: P::area_base,
            area_num: P::area_num,
            reg_read: P::reg_read,
            with_irqsave: with_irqsave::<G>,
        };

        match PERCPU_RUNTIME_STATE.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                unsafe {
                    (*PERCPU_RUNTIME.0.get()).write(runtime);
                }
                PERCPU_RUNTIME_STATE.store(2, Ordering::Release);
            }
            Err(1) => {
                while PERCPU_RUNTIME_STATE.load(Ordering::Acquire) == 1 {
                    spin_loop();
                }
            }
            Err(2) => {}
            Err(_) => unreachable!(),
        }
    }

    #[inline]
    fn runtime() -> &'static Self {
        if PERCPU_RUNTIME_STATE.load(Ordering::Acquire) != 2 {
            panic!("PerCpuRuntime is not initialized");
        }
        unsafe { (*PERCPU_RUNTIME.0.get()).assume_init_ref() }
    }

    #[inline]
    fn id(&self) -> usize {
        (self.id)()
    }

    #[inline]
    fn area_size(&self) -> usize {
        (self.area_size)()
    }

    #[inline]
    fn area_base(&self, cpu_id: usize) -> usize {
        (self.area_base)(cpu_id)
    }

    #[inline]
    fn area_num(&self) -> usize {
        (self.area_num)()
    }

    #[inline]
    fn reg_read(&self) -> usize {
        (self.reg_read)()
    }

    #[inline]
    fn with_irqsave(&self, f: &mut dyn FnMut()) {
        (self.with_irqsave)(f);
    }
}

struct PerCpuRuntimeSlot(UnsafeCell<MaybeUninit<PerCpuRuntime>>);
unsafe impl Sync for PerCpuRuntimeSlot {}

static PERCPU_RUNTIME_STATE: AtomicU8 = AtomicU8::new(0);
static PERCPU_RUNTIME: PerCpuRuntimeSlot =
    PerCpuRuntimeSlot(UnsafeCell::new(MaybeUninit::uninit()));

/// A wrapper for per-CPU static variables.
///
/// `PerCpuVar` provides architecture-independent access helpers for values
/// stored in the `__DATA,__percpu` template section.
#[repr(transparent)]
pub struct PerCpuVar<T>(T);

impl<T> PerCpuVar<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Get the offset relative to the start of the per-CPU template section.
    #[inline]
    pub fn offset(&self) -> usize {
        let start = unsafe { &PERCPU_TEMPLATE_START as *const u8 as usize };
        (self as *const Self as usize) - start
    }

    #[inline]
    fn template_len() -> usize {
        let start = unsafe { &PERCPU_TEMPLATE_START as *const u8 };
        let end = unsafe { &PERCPU_TEMPLATE_END as *const u8 };
        unsafe { end.offset_from(start) as usize }
    }

    #[inline]
    fn template_area_offset() -> usize {
        PerCpuRuntime::runtime().area_size() - Self::template_len()
    }

    /// Returns the raw pointer of this per-CPU static variable on the current
    /// CPU.
    ///
    /// # Safety
    ///
    /// Caller must ensure that preemption is disabled on the current CPU.
    #[inline]
    pub unsafe fn current_ptr(&self) -> *const T {
        (PerCpuRuntime::runtime().reg_read() + Self::template_area_offset() + self.offset())
            as *const T
    }

    /// Returns the reference of the per-CPU static variable on the current CPU.
    ///
    /// # Safety
    ///
    /// Caller must ensure that preemption is disabled on the current CPU.
    #[inline]
    pub unsafe fn current_ref_raw(&self) -> &T {
        unsafe { &*self.current_ptr() }
    }

    /// Returns the mutable reference of the per-CPU static variable on the
    /// current CPU.
    ///
    /// # Safety
    ///
    /// Caller must ensure that preemption is disabled on the current CPU.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn current_ref_mut_raw(&self) -> &mut T {
        unsafe { &mut *(self.current_ptr() as *mut T) }
    }

    /// Manipulate the per-CPU data on the current CPU in the given closure.
    /// Preemption will be disabled during the call.
    #[inline]
    pub fn with_current<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut f = Some(f);
        let mut result = None;
        PerCpuRuntime::runtime().with_irqsave(&mut || {
            result = Some(f.take().unwrap()(unsafe { self.current_ref_mut_raw() }));
        });
        result.expect("percpu with_current callback did not run")
    }

    /// Returns the raw pointer of this per-CPU static variable on the given
    /// CPU.
    ///
    /// # Safety
    ///
    /// Caller must ensure that
    /// - the CPU ID is valid, and
    /// - data races will not happen.
    #[inline]
    pub unsafe fn remote_ptr(&self, cpu_id: usize) -> Option<*const T> {
        if cpu_id >= PerCpuRuntime::runtime().area_num() {
            return None;
        }
        Some(
            (PerCpuRuntime::runtime().area_base(cpu_id)
                + Self::template_area_offset()
                + self.offset()) as *const T,
        )
    }

    /// Returns the reference of the per-CPU static variable on the given CPU.
    ///
    /// # Safety
    ///
    /// Caller must ensure that
    /// - the CPU ID is valid, and
    /// - data races will not happen.
    #[inline]
    pub unsafe fn remote_ref_raw(&self, cpu_id: usize) -> Option<&T> {
        unsafe { Some(&*self.remote_ptr(cpu_id)?) }
    }

    /// Returns the mutable reference of the per-CPU static variable on the
    /// given CPU.
    ///
    /// # Safety
    ///
    /// Caller must ensure that
    /// - the CPU ID is valid, and
    /// - data races will not happen.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn remote_ref_mut_raw(&self, cpu_id: usize) -> Option<&mut T> {
        unsafe { Some(&mut *(self.remote_ptr(cpu_id)? as *mut T)) }
    }
}
