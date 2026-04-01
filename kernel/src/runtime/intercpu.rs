use alloc::boxed::Box;

use libakarin_machine_core::{
    cpu::PerCpuTrait,
    interrupt::{InterruptControllerTrait, IpiReason, IpiTarget, IrqError},
    sync::{NoOp, ScopedGuard},
};
use libakarin_macros::cpu_local;
use libakarin_sync::{
    asynchronous::{RecvError, SendError, bounded},
    mailbox::{MailboxReceiver, MailboxSender, Message, unbounded_mailbox},
    spin::Once,
};

use crate::{
    RuntimeServices,
    arch::PerCpu,
};

type InterCpuJob = Box<dyn FnOnce() + Send + 'static>;
type InterCpuSender = MailboxSender<InterCpuJob, ()>;
type InterCpuReceiver = MailboxReceiver<InterCpuJob, ()>;

cpu_local! {
    static INTERCPU_MAILBOX_TX: Option<InterCpuSender> = None;
    static INTERCPU_MAILBOX_RX: Option<InterCpuReceiver> = None;
}

static INTERCPU_RUNTIME: Once<InterCpuRuntime, ScopedGuard<NoOp>> = Once::new();

/// Runtime-owned inter-processor mailbox fabric.
pub struct InterCpuRuntime {
    cpu_count: usize,
}

/// Errors returned by kernel CPU-to-CPU messaging.
#[derive(Debug)]
pub enum InterCpuError {
    NotInitialized,
    InvalidCpu(usize),
    MissingMailbox(usize),
    SendClosed,
    ReplyClosed,
    Interrupt(IrqError),
}

impl core::fmt::Display for InterCpuError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotInitialized => f.write_str("inter-CPU runtime is not initialized"),
            Self::InvalidCpu(cpu_id) => write!(f, "invalid CPU id {}", cpu_id),
            Self::MissingMailbox(cpu_id) => write!(f, "missing mailbox for CPU {}", cpu_id),
            Self::SendClosed => f.write_str("inter-CPU mailbox sender is closed"),
            Self::ReplyClosed => f.write_str("inter-CPU reply channel is closed"),
            Self::Interrupt(err) => write!(f, "IPI delivery failed: {:?}", err),
        }
    }
}

impl InterCpuRuntime {
    /// Initialize all per-CPU mailbox endpoints and register the mailbox IPI
    /// handler.
    pub fn initialize(cpu_count: usize) -> Result<&'static Self, InterCpuError> {
        let runtime = Self::build(cpu_count)?;
        INTERCPU_RUNTIME
            .try_init(runtime)
            .map_err(|_| InterCpuError::NotInitialized)?;
        Ok(INTERCPU_RUNTIME.get())
    }

    /// Return the initialized inter-CPU runtime.
    pub fn get() -> Result<&'static Self, InterCpuError> {
        if INTERCPU_RUNTIME.is_initialized() {
            Ok(INTERCPU_RUNTIME.get())
        } else {
            Err(InterCpuError::NotInitialized)
        }
    }

    fn build(cpu_count: usize) -> Result<Self, InterCpuError> {
        for cpu_id in 0..cpu_count {
            let (tx, rx) = unbounded_mailbox::<InterCpuJob, ()>();
            unsafe {
                INTERCPU_MAILBOX_TX
                    .remote_ref_mut_raw(cpu_id)
                    .ok_or(InterCpuError::InvalidCpu(cpu_id))?
                    .replace(tx);
                INTERCPU_MAILBOX_RX
                    .remote_ref_mut_raw(cpu_id)
                    .ok_or(InterCpuError::InvalidCpu(cpu_id))?
                    .replace(rx);
            }
        }

        Ok(Self { cpu_count })
    }

    /// Return the initialized CPU count.
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    fn sender_for(&self, cpu_id: usize) -> Result<InterCpuSender, InterCpuError> {
        if cpu_id >= self.cpu_count {
            return Err(InterCpuError::InvalidCpu(cpu_id));
        }

        let sender = unsafe {
            INTERCPU_MAILBOX_TX
                .remote_ref_raw(cpu_id)
                .ok_or(InterCpuError::InvalidCpu(cpu_id))?
                .clone()
        };
        sender.ok_or(InterCpuError::MissingMailbox(cpu_id))
    }

    /// Submit one remote notification job.
    pub async fn notify<F>(&self, cpu_id: usize, f: F) -> Result<(), InterCpuError>
    where
        F: FnOnce() + Send + 'static,
    {
        if cpu_id == PerCpu::id() {
            f();
            return Ok(());
        }

        let sender = self.sender_for(cpu_id)?;
        sender
            .tell(Box::new(f))
            .await
            .map_err(|SendError::Closed(_)| InterCpuError::SendClosed)?;
        RuntimeServices::global()
            .interrupt_controller()
            .send_ipi(IpiReason::Mailbox, IpiTarget::Specific(cpu_id))
            .map_err(InterCpuError::Interrupt)
    }

    /// Submit one remote notification job from synchronous kernel paths.
    pub fn notify_blocking<F>(&self, cpu_id: usize, f: F) -> Result<(), InterCpuError>
    where
        F: FnOnce() + Send + 'static,
    {
        if cpu_id == PerCpu::id() {
            f();
            return Ok(());
        }

        let sender = self.sender_for(cpu_id)?;
        sender
            .tell_blocking(Box::new(f))
            .map_err(|SendError::Closed(_)| InterCpuError::SendClosed)?;
        RuntimeServices::global()
            .interrupt_controller()
            .send_ipi(IpiReason::Mailbox, IpiTarget::Specific(cpu_id))
            .map_err(InterCpuError::Interrupt)
    }

    /// Execute one closure on the target CPU and await its reply.
    pub async fn call<R, F>(&self, cpu_id: usize, f: F) -> Result<R, InterCpuError>
    where
        R: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
    {
        if cpu_id == PerCpu::id() {
            return Ok(f());
        }

        let (tx, rx) = bounded::<R>(1);
        self.notify(cpu_id, move || {
            let _ = tx.try_send(f());
        })
        .await?;
        rx.recv().await.map_err(|_| InterCpuError::ReplyClosed)
    }
}

impl RuntimeServices {
    /// Return the initialized inter-CPU runtime.
    pub fn intercpu(&self) -> Result<&'static InterCpuRuntime, InterCpuError> {
        InterCpuRuntime::get()
    }

    /// Drain and execute all pending mailbox jobs for the current CPU.
    pub fn drain_current_cpu_mailbox(&self) -> usize {
        INTERCPU_MAILBOX_RX.with_current(|slot| {
            let Some(receiver) = slot.as_mut() else {
                return 0;
            };

            let mut drained = 0usize;
            loop {
                match receiver.try_recv() {
                    Ok(Message::Tell(job)) => {
                        drained += 1;
                        job();
                    }
                    Ok(Message::Ask { ret, payload }) => {
                        drained += 1;
                        payload();
                        let _ = ret.try_send(());
                    }
                    Err(RecvError::Empty | RecvError::Closed) => break,
                }
            }
            drained
        })
    }

    /// Initialize the inter-CPU mailbox runtime.
    pub fn init_intercpu(
        &self,
        cpu_count: usize,
    ) -> Result<&'static InterCpuRuntime, InterCpuError> {
        InterCpuRuntime::initialize(cpu_count)
    }
}
