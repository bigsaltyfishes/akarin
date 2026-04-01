# TODO

- [ ] Event system (Requires Tasking crate to be done first)
  - [ ] Object events (on_create, on_destroy, on_update, on_collision, etc)
- [ ] Preemption Safety integration (Require GC preemption safe)
  - [ ] Unsafe methods
    - [ ] WritableMetadata::add_child() (IrqSaveGuard)
    - [ ] WritableMetadata::remove_child() (IrqSaveGuard)
    - [ ] Registry::remove_internal() (IrqSaveGuard)
- [x] Anonymous Object Support (Insert to map, but not in namespace)
