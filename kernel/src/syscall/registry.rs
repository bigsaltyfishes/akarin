//! Built-in syscall registration wiring.
//!
//! This module maps stable syscall numbers to service entry points.

use alloc::{boxed::Box, sync::Arc};

use libakarin_object::ObjectError;
use libakarin_syscall::Syscall;

use super::{SyscallTable, services};

/// Install all built-in syscall handlers into one dispatch table.
pub(super) fn install_builtin_handlers(table: &SyscallTable) -> Result<(), ObjectError> {
    table.register_handler(
        Syscall::HandleClose,
        Arc::new(|args| Box::pin(services::object::handle_close(args))),
    );
    table.register_handler(
        Syscall::HandleClone,
        Arc::new(|args| Box::pin(services::object::handle_clone(args))),
    );
    table.register_handler(
        Syscall::HandleDerive,
        Arc::new(|args| Box::pin(services::object::handle_derive(args))),
    );
    table.register_handler(
        Syscall::ObjectLocate,
        Arc::new(|args| Box::pin(services::object::locate(args))),
    );
    table.register_handler(
        Syscall::ObjectReadMeta,
        Arc::new(|args| Box::pin(services::object::read_meta(args))),
    );
    table.register_handler(
        Syscall::ObjectInvoke,
        Arc::new(|args| Box::pin(services::object::invoke(args))),
    );
    table.register_handler(
        Syscall::TaskYield,
        Arc::new(|args| Box::pin(services::task::yield_now(args))),
    );
    table.register_handler(
        Syscall::TaskSleep,
        Arc::new(|args| Box::pin(services::task::sleep(args))),
    );
    table.register_handler(
        Syscall::PortCreate,
        Arc::new(|args| Box::pin(services::ipc::create(args))),
    );
    table.register_handler(
        Syscall::PortSend,
        Arc::new(|args| Box::pin(services::ipc::send(args))),
    );
    table.register_handler(
        Syscall::PortRecv,
        Arc::new(|args| Box::pin(services::ipc::recv(args))),
    );
    table.register_handler(
        Syscall::PortSubscribe,
        Arc::new(|args| Box::pin(services::ipc::subscribe(args))),
    );
    table.register_handler(
        Syscall::PortUnsubscribe,
        Arc::new(|args| Box::pin(services::ipc::unsubscribe(args))),
    );
    table.register_handler(
        Syscall::PortBindReceiver,
        Arc::new(|args| Box::pin(services::ipc::bind_receiver(args))),
    );
    table.register_handler(
        Syscall::PortRebindReceiver,
        Arc::new(|args| Box::pin(services::ipc::rebind_receiver(args))),
    );
    table.register_handler(
        Syscall::PortQueryState,
        Arc::new(|args| Box::pin(services::ipc::query_state(args))),
    );
    table.register_handler(
        Syscall::PortClose,
        Arc::new(|args| Box::pin(services::ipc::close(args))),
    );
    table.register_handler(
        Syscall::PortFreeze,
        Arc::new(|args| Box::pin(services::ipc::freeze(args))),
    );
    table.register_handler(
        Syscall::IrqWait,
        Arc::new(|args| Box::pin(services::irq::wait(args))),
    );
    table.register_handler(
        Syscall::IrqAck,
        Arc::new(|args| Box::pin(services::irq::ack(args))),
    );
    table.register_handler(
        Syscall::ProcessExit,
        Arc::new(|args| Box::pin(services::process::exit(args))),
    );
    table.register_handler(
        Syscall::ProcessCreate,
        Arc::new(|args| Box::pin(services::process::create(args))),
    );
    table.register_handler(
        Syscall::ProcessLoad,
        Arc::new(|args| Box::pin(services::process::load(args))),
    );
    table.register_handler(
        Syscall::ProcessVmarExtract,
        Arc::new(|args| Box::pin(services::process::vmar_extract(args))),
    );
    table.register_handler(
        Syscall::ProcessHandleInstall,
        Arc::new(|args| Box::pin(services::process::handle_install(args))),
    );
    table.register_handler(
        Syscall::ProcessTaskSpawn,
        Arc::new(|args| Box::pin(services::process::task_spawn(args))),
    );
    table.register_handler(
        Syscall::TaskCreate,
        Arc::new(|args| Box::pin(services::task::create(args))),
    );
    table.register_handler(
        Syscall::TaskExit,
        Arc::new(|args| Box::pin(services::task::exit(args))),
    );
    table.register_handler(
        Syscall::FutexWait,
        Arc::new(|args| Box::pin(services::futex::wait(args))),
    );
    table.register_handler(
        Syscall::FutexWake,
        Arc::new(|args| Box::pin(services::futex::wake(args))),
    );
    table.register_handler(
        Syscall::UserObjectCreate,
        Arc::new(|args| Box::pin(services::service::user_object_create(args))),
    );
    table.register_handler(
        Syscall::WaitObjectRequest,
        Arc::new(|args| Box::pin(services::service::wait_object_request(args))),
    );
    table.register_handler(
        Syscall::ServiceReplyOk,
        Arc::new(|args| Box::pin(services::service::reply_ok(args))),
    );
    table.register_handler(
        Syscall::ServiceReplyObjectError,
        Arc::new(|args| Box::pin(services::service::reply_object_error(args))),
    );
    table.register_handler(
        Syscall::ServiceReplyUnderlying,
        Arc::new(|args| Box::pin(services::service::reply_underlying(args))),
    );
    table.register_handler(
        Syscall::PagerCreate,
        Arc::new(|args| Box::pin(services::service::pager_create(args))),
    );
    table.register_handler(
        Syscall::WaitPagerRequest,
        Arc::new(|args| Box::pin(services::service::wait_pager_request(args))),
    );
    table.register_handler(
        Syscall::SyscallHandlerCreate,
        Arc::new(|args| Box::pin(services::service::syscall_handler_create(args))),
    );
    table.register_handler(
        Syscall::WaitSyscallRequest,
        Arc::new(|args| Box::pin(services::service::wait_syscall_request(args))),
    );
    table.register_handler(
        Syscall::VmoCreate,
        Arc::new(|args| Box::pin(services::vm::vmo_create(args))),
    );
    table.register_handler(
        Syscall::VmoCreateChild,
        Arc::new(|args| Box::pin(services::vm::vmo_create_child(args))),
    );
    table.register_handler(
        Syscall::VmoGetSize,
        Arc::new(|args| Box::pin(services::vm::vmo_get_size(args))),
    );
    table.register_handler(
        Syscall::VmoGetStreamSize,
        Arc::new(|args| Box::pin(services::vm::vmo_get_stream_size(args))),
    );
    table.register_handler(
        Syscall::VmoOpRange,
        Arc::new(|args| Box::pin(services::vm::vmo_op_range(args))),
    );
    table.register_handler(
        Syscall::VmoRead,
        Arc::new(|args| Box::pin(services::vm::vmo_read(args))),
    );
    table.register_handler(
        Syscall::VmoWrite,
        Arc::new(|args| Box::pin(services::vm::vmo_write(args))),
    );
    table.register_handler(
        Syscall::VmoSetSize,
        Arc::new(|args| Box::pin(services::vm::vmo_set_size(args))),
    );
    table.register_handler(
        Syscall::VmoSetCachePolicy,
        Arc::new(|args| Box::pin(services::vm::vmo_set_cache_policy(args))),
    );
    table.register_handler(
        Syscall::VmoSetStreamSize,
        Arc::new(|args| Box::pin(services::vm::vmo_set_stream_size(args))),
    );
    table.register_handler(
        Syscall::VmoTransferData,
        Arc::new(|args| Box::pin(services::vm::vmo_transfer_data(args))),
    );
    table.register_handler(
        Syscall::VmarAllocate,
        Arc::new(|args| Box::pin(services::vm::vmar_allocate(args))),
    );
    table.register_handler(
        Syscall::VmarMap,
        Arc::new(|args| Box::pin(services::vm::vmar_map(args))),
    );
    table.register_handler(
        Syscall::VmarProtect,
        Arc::new(|args| Box::pin(services::vm::vmar_protect(args))),
    );
    table.register_handler(
        Syscall::VmarUnmap,
        Arc::new(|args| Box::pin(services::vm::vmar_unmap(args))),
    );
    table.register_handler(
        Syscall::VmarDestroy,
        Arc::new(|args| Box::pin(services::vm::vmar_destroy(args))),
    );
    Ok(())
}
