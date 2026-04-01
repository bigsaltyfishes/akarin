# Akarin

Akarin is a research microkernel project written in Rust. The current codebase focuses on a capability-oriented object model, a unified async scheduler, userspace service dispatch, and extensible virtual memory abstractions rather than POSIX compatibility.

The project has already crossed the "first userspace process boots successfully" milestone. The kernel, bootloader, object system, garbage collector, virtual memory subsystem, userspace pager, and userspace syscall handler are all present in the current tree.

## Project Overview

Akarin is organized as a Rust workspace instead of a single kernel crate:

- `bootloader/`
  - UEFI bootloader and boot protocol crates
  - loads PIE kernel images and packages boot assets into an ESP
- `kernel/`
  - the kernel binary
  - contains scheduling, traps, syscall services, service dispatch, VM, IPC, interrupt handling, and bootstrap userspace startup
- `library/`
  - reusable crates that define the system's shared foundations
  - includes `core`, `machine`, `sync`, `object`, `collections`, `device`, `runtime`, and `dyld`

At a high level, the system is built around these ideas:

- Capability-oriented object system
  - a hierarchical object namespace, typed control planes, guarded dispatch, and derived handles
- Unified scheduler model
  - one `Scheduler` per CPU, one `Task` abstraction for both kernel and user execution
- `VMO / VMAR / VSpace`
  - Zircon-like memory primitives with COW, pager-backed faults, and process-owned address spaces
- Userspace kernel services
  - general `UserObject`s plus special `PagerObject` and `SyscallHandlerObject`
- Trait-based ISA abstraction
  - x86_64-specific behavior is wired through `library/machine` contracts instead of leaking ad-hoc architecture helpers into the rest of the kernel

## Current Status

The current tree already implements the following major pieces:

- Boot and image loading
  - custom UEFI bootloader
  - PIE kernel image loading
  - Mach-O / Fat executable loading for bootstrap userspace programs
- Object and capability system
  - registry-backed hierarchical object namespace
  - anonymous and discoverable objects
  - handle derivation, capability masking, interface capabilities, and guarded control-plane dispatch
- Concurrent runtime foundations
  - Hyaline-1-style concurrent GC runtime in `library/sync`
  - async events, mailbox primitives, MPMC channels, and project-owned spin locks
- Scheduling and task runtime
  - single per-CPU `Scheduler`
  - EEVDF scheduling
  - unified task model for kernel and userspace execution
  - SIMD/FPU save/restore on x86_64
- Virtual memory
  - `VMO / VMAR / VSpace`
  - child/shared/COW `VMO`
  - pager-backed page faults
  - explicit user layout segments such as `UserImage`, `UserMapped`, `UserHeap`, `UserStack`, and `UserMmio`
- Userspace service dispatch
  - `UserObject`
  - `PagerObject`
  - `SyscallHandlerObject`
  - L4-style fast-path service delivery without spawning helper tasks
- IPC and synchronization
  - port IPC
  - futex runtime
  - userspace runtime allocator and bootstrap support
- x86_64 machine support
  - trap handling
  - APIC/IOAPIC interrupt controller
  - timer wiring
  - SMP bootstrap

Work still planned next is documented in `TODO.md`, with the current near-term focus on:

- `wait / join`
- service cleanup and unregister flows
- single-task `exec`
- final fault / exit policy
- userspace driver control planes for IRQ / PCI / MMIO / DMA

## Repository Layout

- `bootloader/`
  - UEFI bootloader and boot protocol
- `kernel/`
  - kernel binary and subsystem implementations
- `library/core/`
  - ISA-neutral core abstractions such as `VMO`, `VMAR`, `VSpace`, clock types, and MMIO wrappers
- `library/machine/`
  - machine-level traits and architecture bindings
- `library/object/`
  - object tree, handles, capabilities, guarded dispatch, and registry
- `library/sync/`
  - GC runtime, async event primitives, MPMC channels, mailbox, and project-owned locks
- `library/collections/`
  - reusable concurrent and intrusive data structures
- `library/dyld/`
  - dynamic linking and relocation helpers used by image loading
- `template/`
  - local boot assets such as `OVMF.fd` and `boot.cfg`

## Build and Run

The repository uses custom target specifications for the bootloader, kernel, and userspace bootstrap program.

### Common commands

Use `just` from the repository root:

- `just check`
  - type-check the kernel, bootloader, and bootstrap userspace program
- `just build`
  - build bootloader, kernel, and bootstrap program
- `just build-esp`
  - package an EFI System Partition under `target/esp`
- `just run`
  - boot the system in QEMU with KVM acceleration
- `just clean`
  - remove build artifacts

### Target layout

- kernel target
  - `x86_64-unknown-none-macho.json`
- userspace target
  - `x86_64-unknown-akarin.json`
- bootloader target
  - `x86_64-unknown-uefi-debug.json`

### Direct cargo examples

```bash
cargo check -Zjson-target-spec -p akarin_kernel --target ./x86_64-unknown-none-macho.json
cargo check -Zjson-target-spec -p bootstrap --target ./x86_64-unknown-akarin.json
cargo check -Zjson-target-spec -p akarin-bootloader --target ./x86_64-unknown-uefi-debug.json
```

If you use Nix, enter the development shell first:

```bash
nix develop
```

## Verification

Typical verification for kernel-facing changes is:

- `cargo fmt --all`
- `just check`
- `just run`

For focused crate validation, use standard Cargo commands on the relevant crate, for example:

```bash
cargo test -p libakarin-object
HOST=$(rustc -vV | sed -n 's/^host: //p'); cargo -Zbuild-std test -p libakarin-sync --target "$HOST"
```

## License and Status

Akarin is still under active research and development. Interfaces and internal subsystem layouts may continue to change as the remaining lifecycle, driver, and userspace service work is completed.
