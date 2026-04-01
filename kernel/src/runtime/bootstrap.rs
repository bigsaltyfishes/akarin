use core::{cell::UnsafeCell, mem};

use libakarin_machine_core::{
    memory::FrameAllocatorTrait,
    sync::{NoOp, ScopedGuard},
};
use libakarin_object::{ObjectError, Registry};
use libakarin_sync::{
    gc::{Collector, LocalHandle, global_collector, install_global_collector, set_current_local},
    spin::Once,
};

use crate::{NamespaceBootstrap, NamespaceSet, RuntimeServices};

static OBJECT_REGISTRY: Once<Registry, ScopedGuard<NoOp>> = Once::new();
static NAMESPACE_SET: Once<NamespaceSet, ScopedGuard<NoOp>> = Once::new();
static GC_EARLY_LOCAL: LocalSlot = LocalSlot::new();

struct LocalSlot(UnsafeCell<Option<LocalHandle>>);

unsafe impl Sync for LocalSlot {}

impl LocalSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    fn set(&self, value: LocalHandle) {
        unsafe { *self.0.get() = Some(value) }
    }

    fn clear(&self) {
        unsafe {
            let _ = mem::take(&mut *self.0.get());
        }
    }
}

struct GcBootstrap;

impl GcBootstrap {
    fn init_early(possible_cpu_num: usize) {
        let collector = install_global_collector(Collector::new(possible_cpu_num));
        let local = collector.register();
        set_current_local(local.clone());
        GC_EARLY_LOCAL.set(local);
    }

    fn promote_runtime_local() {
        let collector = global_collector();
        let runtime_local = collector.register();
        set_current_local(runtime_local);
        GC_EARLY_LOCAL.clear();
    }
}

/// Early bootstrap context exposed to kernel-specific probe hooks.
pub struct BootstrapContext {
    registry: &'static Registry,
    namespaces: &'static NamespaceSet,
}

impl BootstrapContext {
    /// Return the global object registry prepared during bootstrap.
    pub fn registry(&self) -> &'static Registry {
        self.registry
    }

    /// Return the canonical namespace set prepared during bootstrap.
    pub fn namespaces(&self) -> &'static NamespaceSet {
        self.namespaces
    }
}

/// Single-use runtime bootstrap entry for early kernel bringup.
///
/// This object owns the bootstrap sequence for runtime-wide state:
/// boot-info publication, early GC setup, root registry initialization,
/// namespace creation, and final runtime service installation.
pub struct RuntimeBootstrap;

impl RuntimeBootstrap {
    /// Publish boot info and initialize runtime-wide state.
    ///
    /// `bootstrap_hook` runs after the registry and namespaces are ready but
    /// before runtime services are finalized. Kernel-specific device probing
    /// should be performed in this hook.
    pub fn initialize<F>(
        boot_info: &'static mut libakarin_boot_proto::BootInfo,
        possible_cpu_num: usize,
        frame_allocator: &'static dyn FrameAllocatorTrait,
        bootstrap_hook: F,
    ) -> Result<&'static RuntimeServices, ObjectError>
    where
        F: FnOnce(&BootstrapContext),
    {
        RuntimeServices::install_boot_info(boot_info as *mut _);
        GcBootstrap::init_early(possible_cpu_num);

        let registry = OBJECT_REGISTRY.get_or_else(Registry::new);
        let root_super = registry.init_root()?;
        let namespaces = NamespaceBootstrap::new(root_super).initialize()?;
        NAMESPACE_SET.init(namespaces);
        NAMESPACE_SET
            .get()
            .bootloader_manager()
            .publish_boot_info_resources(boot_info)?;

        let context = BootstrapContext {
            registry,
            namespaces: NAMESPACE_SET.get(),
        };
        bootstrap_hook(&context);

        let runtime = RuntimeServices::install(RuntimeServices::new(
            registry,
            frame_allocator,
            context.namespaces(),
        ));
        GcBootstrap::promote_runtime_local();

        Ok(runtime)
    }
}
