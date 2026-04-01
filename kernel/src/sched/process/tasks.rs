use super::*;

impl Process {
    /// Record that a task belongs to this process.
    ///
    /// Once a process has entered terminating state, no new tasks may be
    /// attached.
    pub fn register_task(&self, task: &Arc<Task>) -> Result<(), ObjectError> {
        let mut state = self.tasks.lock();
        if state.terminating {
            return Err(ObjectError::ObjectDestroyed);
        }

        let task_id = task.id();
        if state.tasks.insert(task_id, Arc::downgrade(task)).is_some() {
            return Err(ObjectError::InvalidArgument);
        }
        Ok(())
    }

    /// Remove one previously registered task without changing process exit
    /// state. Spawn uses this to roll back a failed publish after registration
    /// succeeded but before the task became runnable.
    pub fn unregister_task(&self, task_id: TaskId) -> Result<(), ObjectError> {
        let mut state = self.tasks.lock();
        if state.tasks.remove(&task_id).is_none() {
            return Err(ObjectError::ObjectNotFound);
        }

        Ok(())
    }

    /// Remove a task from this process and report whether explicit process
    /// exit requires process-wide cancellation.
    pub fn reap_task(
        &self,
        task_id: TaskId,
        exit: TaskExit,
    ) -> Result<ProcessReapAction, ObjectError> {
        if let TaskExit::ProcessExited(code) = exit {
            let mut state = self.tasks.lock();
            if state.tasks.remove(&task_id).is_none() {
                return Err(ObjectError::ObjectNotFound);
            }
            if state.terminating {
                if state.tasks.is_empty() {
                    self.mark_exited();
                }
                return Ok(ProcessReapAction::Detached);
            }

            state.terminating = true;
            state.exit_code = Some(code);
            if state.tasks.is_empty() {
                self.mark_exited();
            } else {
                self.mark_terminating();
            }
            let cancel = state.tasks.keys().copied().collect();
            let sibling_tasks = state
                .tasks
                .values()
                .filter_map(|task| task.upgrade())
                .collect::<Vec<_>>();
            drop(state);
            self.exit_event.notify_all();

            // Process exit must also reach tasks that have been registered but
            // whose initial runnable has not been published yet.
            for sibling in sibling_tasks {
                sibling.request_cancel();
            }
            return Ok(ProcessReapAction::ProcessExited { code, cancel });
        }

        let mut state = self.tasks.lock();
        if state.tasks.remove(&task_id).is_none() {
            return Err(ObjectError::ObjectNotFound);
        }
        if state.terminating && state.tasks.is_empty() {
            self.mark_exited();
        }
        Ok(ProcessReapAction::Detached)
    }

    /// Return whether the process currently owns the supplied task id.
    pub fn owns_task_id(&self, task_id: TaskId) -> bool {
        self.tasks.lock().tasks.contains_key(&task_id)
    }

    /// Return the number of registered tasks in this process.
    pub fn task_count(&self) -> usize {
        self.tasks.lock().tasks.len()
    }

    /// Let the process decide how to react to one userspace trap.
    pub fn handle_fault(&self, reason: &TrapReason) -> ProcessFaultAction {
        match reason {
            TrapReason::SoftwareBreakpoint
            | TrapReason::HardwareBreakpoint
            | TrapReason::Interrupt(_) => ProcessFaultAction::Resume,
            TrapReason::Syscall
            | TrapReason::PageFault(_, _)
            | TrapReason::UndefinedInstruction
            | TrapReason::UnalignedAccess
            | TrapReason::GeneralFault(_) => ProcessFaultAction::Terminate,
        }
    }
}
