# TODO

- [ ] Garbage Collector
    - [ ] Preemptable RCU (Require Task system to be finished)
        - [ ] Add `online_readers` and `blocked_readers` list into `Local`, they mast be `Atomic`
        - [ ] Introduce IrqGuard for operations on `batch` and `sealed_batch` field.
        - [ ] Introduce `ThreadLocal`
            - [ ] Move `guard_count` from `Local` to `ThreadLocal`
            - [ ] Bind thread into specific CPU
        - [ ] Remove INACTIVE tag, we assume a `Local` is active when `online_readers` != 0 && `blocked_readers` != ptr::nullmut() 
    - [ ] Seperate RCU Cleanup logic into a worker (Require Task system to be finished)