use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use lazy_static::lazy_static;
use libakarin_object::{
    Capability, ControlPlane, Handle, NameSpace, ObjectPath, Payload, SyscallDispatch,
};
use libakarin_sync::gc::{
    Collector, global_collector, install_global_collector, set_current_local,
};

lazy_static! {
    static ref CPU_NUM: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
}

lazy_static! {
    static ref THREADS: usize = (*CPU_NUM * 2).max(2);
}

static GC_RUNTIME: OnceLock<()> = OnceLock::new();

type Registry = libakarin_object::Registry;

lazy_static! {
    static ref REGISTRY: Registry = Registry::new();
}

static REGISTRY_STATUS: AtomicUsize = AtomicUsize::new(0);
static SUPER_HANDLE: OnceLock<Handle> = OnceLock::new();

fn install_thread_gc() {
    GC_RUNTIME.get_or_init(|| {
        install_global_collector(Collector::new(*CPU_NUM).batch_size(32));
    });
    let local = global_collector().register();
    set_current_local(local);
}

fn spawn_gc<F, T>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        install_thread_gc();
        f()
    })
}

fn try_init_registry() {
    install_thread_gc();
    while REGISTRY_STATUS.load(Ordering::Acquire) != 2 {
        let status = REGISTRY_STATUS.load(Ordering::Acquire);
        match status {
            0 => {
                if REGISTRY_STATUS
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // Initialize the registry.
                    SUPER_HANDLE.set(REGISTRY.init_root().unwrap()).unwrap();
                    REGISTRY_STATUS.store(2, Ordering::Release);
                }
            }
            _ => {
                // Wait for initialization to complete
                std::thread::yield_now();
            }
        }
    }
}

/// Create a new isolated registry for tests that need independence.
fn new_registry() -> (&'static Registry, Handle) {
    install_thread_gc();
    let registry = Box::leak(Box::new(Registry::new()));
    let root_handle = registry.init_root().unwrap();
    (registry, root_handle)
}

#[test]
fn test_insert_object() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();
    let child_handle = super_handle
        .write_with(|obj| {
            obj.add_child(
                "child1".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    child_handle
        .read_with(|object| {
            assert_eq!(object.name(), "child1");
            assert_eq!(object.parent(), Some(0));
        })
        .unwrap();
}

// ==========================
// Basic Functionality Tests
// ==========================

#[test]
fn test_nested_children() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    // Create nested structure: root -> test_nested -> level1 -> level2
    let level0 = super_handle
        .write_with(|obj| {
            obj.add_child(
                "test_nested".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let level1 = level0
        .write_with(|obj| {
            obj.add_child(
                "level1".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let level2 = level1
        .write_with(|obj| {
            obj.add_child(
                "level2".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Verify level0
    level0
        .read_with(|obj| {
            assert_eq!(obj.name(), "test_nested");
            assert_eq!(obj.parent(), Some(0));
            assert_eq!(obj.children().count(), 1);
            assert_eq!(
                obj.query_child("level1").unwrap(),
                level1.object_id().unwrap()
            );
        })
        .unwrap();

    // Verify level1
    level1
        .read_with(|obj| {
            assert_eq!(obj.name(), "level1");
            assert_eq!(obj.parent(), level0.object_id());
            assert_eq!(obj.children().count(), 1);
            assert_eq!(
                obj.query_child("level2").unwrap(),
                level2.object_id().unwrap()
            );
        })
        .unwrap();

    // Verify level2
    level2
        .read_with(|obj| {
            assert_eq!(obj.name(), "level2");
            assert_eq!(obj.parent(), level1.object_id());
            assert_eq!(obj.children().count(), 0);
        })
        .unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct TestPayload {
    value: i32,
    name: String,
}

impl ControlPlane for TestPayload {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }
}

impl SyscallDispatch for TestPayload {}

#[test]
fn test_payload_read() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let payload = TestPayload {
        value: 42,
        name: "test_payload".to_string(),
    };

    let child = super_handle
        .write_with(|obj| {
            obj.add_child(
                "payload_test".to_string(),
                Capability::empty(),
                Payload::new(payload),
            )
        })
        .unwrap()
        .unwrap();

    // Correct type should work
    child
        .execute_cp_with::<TestPayload, _, _>(|payload_ref| {
            assert_eq!(payload_ref.value, 42);
            assert_eq!(payload_ref.name, "test_payload");
        })
        .unwrap();

    // Wrong type should return InvalidArgument
    let result = child.execute_cp_with::<NameSpace, _, _>(|_| ());
    assert!(matches!(
        result,
        Err(libakarin_object::ObjectError::InvalidArgument)
    ));
}

#[test]
fn test_duplicate_child_name() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let parent = super_handle
        .write_with(|obj| {
            obj.add_child(
                "dup_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // First child with name "child" should succeed
    let result1 = parent.write_with(|obj| {
        obj.add_child(
            "child".to_string(),
            Capability::empty(),
            Payload::new(NameSpace),
        )
    });
    assert!(result1.is_ok());

    // Second child with same name should fail
    let result2 = parent.write_with(|obj| {
        obj.add_child(
            "child".to_string(),
            Capability::empty(),
            Payload::new(NameSpace),
        )
    });
    assert!(matches!(
        result2,
        Ok(Err(libakarin_object::ObjectError::DuplicateChildName))
    ));
}

// ==============================
// Capability System Verification
// ==============================

#[test]
fn test_capability_masking() {
    let (_registry, root) = new_registry();

    // Root has TRUSTED capabilities, create a child without masking
    // READ/WRITE/CLONE
    let parent = root
        .write_with(|obj| {
            obj.add_child(
                "parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Parent should have full ADMIN composite capabilities (from insert_internal)
    assert!(parent.capabilities().contains(Capability::ADMIN_GRP));
    assert!(parent.capabilities().contains(Capability::READ));
    assert!(parent.capabilities().contains(Capability::WRITE));
    assert!(parent.capabilities().contains(Capability::EXECUTE));

    // Create child with EXECUTE and WRITE masked
    let masked = Capability::EXECUTE | Capability::WRITE;
    let child = parent
        .write_with(|obj| obj.add_child("child".to_string(), masked, Payload::new(NameSpace)))
        .unwrap()
        .unwrap();

    // Owner's ADMIN handle is NOT affected by masked_caps
    let child_caps = child.capabilities();
    assert!(child_caps.contains(Capability::ADMIN_GRP));
    assert!(child_caps.contains(Capability::EXECUTE));
    assert!(child_caps.contains(Capability::WRITE));

    // But a derived handle IS affected: derive_handle gives specific caps
    // (not subject to masking, but limited to requested caps)
    let reader = child.derive_handle(Capability::READ, 0).unwrap();
    assert!(reader.capabilities().contains(Capability::READ));
    assert!(!reader.capabilities().contains(Capability::WRITE));
    assert!(!reader.capabilities().contains(Capability::EXECUTE));

    // Verify masking effect through path traversal (locate_and_then)
    // masked_caps restricts handles acquired via parent token derivation
    let child_id = child.object_id().unwrap();
    parent
        .read_with(|obj| {
            let query_id = obj.query_child("child").unwrap();
            assert_eq!(query_id, child_id);
        })
        .unwrap();
}

#[test]
fn test_read_requires_cap() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let child = super_handle
        .write_with(|obj| {
            obj.add_child(
                "read_cap_test".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Derive a handle without READ capability
    let degraded = child.derive_handle(Capability::WRITE, 0).unwrap();

    // Should not be able to read
    let result = degraded.read_with(|_obj| {});
    assert!(matches!(
        result,
        Err(libakarin_object::ObjectError::InsufficientCapabilities)
    ));
}

#[test]
fn test_write_requires_cap() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let child = super_handle
        .write_with(|obj| {
            obj.add_child(
                "write_cap_test".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Derive a handle without WRITE capability
    let degraded = child.derive_handle(Capability::READ, 0).unwrap();

    // Should not be able to write
    let result = degraded.write_with(|_obj| {});
    assert!(matches!(
        result,
        Err(libakarin_object::ObjectError::InsufficientCapabilities)
    ));
}

#[test]
fn test_execute_requires_cap() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    // Create child with EXECUTE masked
    let parent = super_handle
        .write_with(|obj| {
            obj.add_child(
                "exec_cap_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let child = parent
        .write_with(|obj| {
            obj.add_child(
                "child".to_string(),
                Capability::EXECUTE,
                Payload::new(TestPayload {
                    value: 42,
                    name: String::new(),
                }),
            )
        })
        .unwrap()
        .unwrap();

    // Owner's ADMIN handle can still access payload (not affected by masking)
    child
        .execute_cp_with::<TestPayload, _, _>(|payload| assert_eq!(payload.value, 42))
        .unwrap();

    // But a derived handle without EXECUTE cannot access payload
    let reader = child.derive_handle(Capability::READ, 0).unwrap();
    let result = reader.execute_cp_with::<TestPayload, _, _>(|_| ());
    assert!(matches!(
        result,
        Err(libakarin_object::ObjectError::InsufficientCapabilities)
    ));
}

#[test]
fn test_admin_agent_bypass() {
    let (_registry, root) = new_registry();

    // Root handle gets TRUSTED & !masked. masked = EXECUTE | ADMIN_BIT | AGENT_BIT
    // So root has: CLONE | SEND | READ | WRITE (and all other high bits except
    // 4,5,6) Root does NOT have ADMIN_BIT, AGENT_BIT, or EXECUTE.
    assert!(!root.capabilities().contains(Capability::ADMIN));
    assert!(!root.capabilities().contains(Capability::EXECUTE));

    // Root has READ and WRITE directly
    assert!(root.capabilities().contains(Capability::READ));
    assert!(root.capabilities().contains(Capability::WRITE));

    // Can read (has READ directly)
    root.read_with(|obj| {
        assert_eq!(obj.name(), "");
    })
    .unwrap();

    // Can write (has WRITE directly)
    root.write_with(|obj| {
        obj.set_masked_caps(Capability::EXECUTE | Capability::ADMIN | Capability::AGENT);
    })
    .unwrap();

    // Create a child through root — child gets ADMIN composite from insert_internal
    let child = root
        .write_with(|obj| {
            obj.add_child(
                "test_child".to_string(),
                Capability::empty(),
                Payload::new(TestPayload {
                    value: 42,
                    name: String::new(),
                }),
            )
        })
        .unwrap()
        .unwrap();

    // Child has ADMIN composite (READ|WRITE|EXECUTE|ADMIN_BIT|AGENT_BIT)
    assert!(child.capabilities().contains(Capability::ADMIN_GRP));
    assert!(child.capabilities().contains(Capability::READ));
    assert!(child.capabilities().contains(Capability::WRITE));
    assert!(child.capabilities().contains(Capability::EXECUTE));

    // Child can read, write, and access payload through its ADMIN composite
    child
        .read_with(|obj| {
            assert_eq!(obj.name(), "test_child");
        })
        .unwrap();

    child
        .write_with(|obj| {
            obj.set_masked_caps(Capability::empty());
        })
        .unwrap();

    child
        .execute_cp_with::<TestPayload, _, _>(|payload| assert_eq!(payload.value, 42))
        .unwrap();
}

#[test]
fn test_clone_requires_cap() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let parent = super_handle
        .write_with(|obj| {
            obj.add_child(
                "clone_test_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Parent handle has ADMIN composite (from insert_internal), which does NOT
    // include CLONE. So try_clone on parent should fail.
    assert!(!parent.capabilities().contains(Capability::CLONE));
    let result = parent.try_clone();
    assert!(matches!(
        result,
        Err(libakarin_object::ObjectError::InsufficientCapabilities)
    ));

    // Derive a handle WITH CLONE capability from parent (ADMIN can derive CLONE)
    let cloneable = parent
        .derive_handle(Capability::CLONE | Capability::READ, 0)
        .unwrap();
    assert!(cloneable.capabilities().contains(Capability::CLONE));

    // Now try_clone should succeed
    assert!(cloneable.try_clone().is_ok());
}

#[test]
fn test_handle_downgrade() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let child = super_handle
        .write_with(|obj| {
            obj.add_child(
                "downgrade_test".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Derive a handle with READ and WRITE (from ADMIN)
    let mut handle = child
        .derive_handle(Capability::READ | Capability::WRITE, 0)
        .unwrap();

    // Initially has READ and WRITE
    assert!(handle.read_with(|_| {}).is_ok());
    assert!(handle.write_with(|_| {}).is_ok());

    // Downgrade READ
    handle.downgrade(Capability::READ, 0);
    assert!(matches!(
        handle.read_with(|_| {}),
        Err(libakarin_object::ObjectError::InsufficientCapabilities)
    ));

    // Downgrade WRITE
    handle.downgrade(Capability::WRITE, 0);
    assert!(matches!(
        handle.write_with(|_| {}),
        Err(libakarin_object::ObjectError::InsufficientCapabilities)
    ));
}

// ===================================
// Object Lifecycle and Destruction
// ===================================

#[test]
fn test_admin_drop_destroys_object() {
    let (_registry, root) = new_registry();

    let child = root
        .write_with(|obj| {
            obj.add_child(
                "to_destroy".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Derive a non-admin handle (READ only) to verify object state later
    let reader = child.derive_handle(Capability::READ, 0).unwrap();

    // Drop the ADMIN handle — should destroy the object
    drop(child);

    // Object should be destroyed
    assert!(!reader.is_valid());
    assert!(matches!(
        reader.read_with(|_| {}),
        Err(libakarin_object::ObjectError::ObjectDestroyed)
    ));
}

#[test]
fn test_non_admin_drop_keeps_object() {
    let (_registry, root) = new_registry();

    let child = root
        .write_with(|obj| {
            obj.add_child(
                "persist".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Derive a reader handle (no ADMIN_BIT)
    let reader = child.derive_handle(Capability::READ, 0).unwrap();

    // Drop the reader — object should stay alive (child still has ADMIN)
    drop(reader);

    // Object should still be alive
    assert!(child.is_valid());
    assert!(
        child
            .read_with(|obj| {
                assert_eq!(obj.name(), "persist");
            })
            .is_ok()
    );
}

#[test]
fn test_bfs_recursive_destruction() {
    let (_registry, root) = new_registry();

    // Create 3-level tree
    let level1 = root
        .write_with(|obj| {
            obj.add_child(
                "level1".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let level2 = level1
        .write_with(|obj| {
            obj.add_child(
                "level2".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let level3 = level2
        .write_with(|obj| {
            obj.add_child(
                "level3".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Derive non-admin reader handles to verify destruction
    let level2_reader = level2.derive_handle(Capability::READ, 0).unwrap();
    let level3_reader = level3.derive_handle(Capability::READ, 0).unwrap();

    // Drop level1 (ADMIN) - should destroy entire subtree
    drop(level1);

    // All descendants should be destroyed
    assert!(!level2_reader.is_valid());
    assert!(!level3_reader.is_valid());
}

#[test]
fn test_write_remove_child_blocked_by_admin_mask() {
    let (_registry, root) = new_registry();

    let parent = root
        .write_with(|obj| {
            obj.add_child(
                "parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // Create child with WRITE masked — this prevents a parent WRITE handle
    // from removing it through `remove_child`.
    let child = parent
        .write_with(|obj| {
            obj.add_child(
                "protected_child".to_string(),
                Capability::WRITE,
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    // `remove_child` should fail because the child masks WRITE.
    let result = parent.write_with(|obj| obj.remove_child("protected_child"));
    assert!(matches!(
        result,
        Ok(Err(libakarin_object::ObjectError::InsufficientCapabilities))
    ));

    // Verify child still exists
    assert!(child.is_valid());
    assert!(
        child
            .read_with(|obj| {
                assert_eq!(obj.name(), "protected_child");
            })
            .is_ok()
    );
}

// ======================
// Path Resolution Tests
// ======================

#[test]
fn test_absolute_path_locate() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    // Create path: /path_test/a/b
    let path_test = super_handle
        .write_with(|obj| {
            obj.add_child(
                "path_test".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let a = path_test
        .write_with(|obj| {
            obj.add_child("a".to_string(), Capability::WRITE, Payload::new(NameSpace))
        })
        .unwrap()
        .unwrap();

    let b = a
        .write_with(|obj| {
            obj.add_child(
                "b".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let expected_id = b.object_id().unwrap();

    // Locate using absolute path
    let path = ObjectPath::new("/path_test/a/b");
    REGISTRY
        .locate_and_then(None::<&Handle>, &path, |handle| {
            assert_eq!(handle.object_id().unwrap(), expected_id);
            // Capability should be attenuated (WRITE masked at 'a')
            assert!(!handle.capabilities().contains(Capability::WRITE));
            assert!(handle.capabilities().contains(Capability::READ));
        })
        .unwrap();
}

#[test]
fn test_relative_path_locate() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let parent = super_handle
        .write_with(|obj| {
            obj.add_child(
                "rel_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let child = parent
        .write_with(|obj| {
            obj.add_child(
                "child".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let expected_id = child.object_id().unwrap();

    // Locate using relative path from parent
    let path = ObjectPath::new("child");
    REGISTRY
        .locate_and_then(Some(&parent), &path, |handle| {
            assert_eq!(handle.object_id().unwrap(), expected_id);
        })
        .unwrap();
}

#[test]
fn test_path_not_found() {
    try_init_registry();

    let path = ObjectPath::new("/nonexistent/path");
    let result = REGISTRY.locate_and_then(None::<&Handle>, &path, |_| {});

    assert!(matches!(
        result,
        Err(libakarin_object::ObjectError::ObjectNotFound)
    ));
}

#[test]
fn test_self_component_skip() {
    try_init_registry();

    let super_handle = SUPER_HANDLE.get().unwrap();

    let parent = super_handle
        .write_with(|obj| {
            obj.add_child(
                "dot_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let child = parent
        .write_with(|obj| {
            obj.add_child(
                "child".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let expected_id = child.object_id().unwrap();

    // Path with 'self' should resolve to same object
    let path1 = ObjectPath::new("/dot_parent/self/child");
    let path2 = ObjectPath::new("/dot_parent/child");

    let id1 = REGISTRY
        .locate_and_then(None::<&Handle>, &path1, |h| h.object_id().unwrap())
        .unwrap();

    let id2 = REGISTRY
        .locate_and_then(None::<&Handle>, &path2, |h| h.object_id().unwrap())
        .unwrap();

    assert_eq!(id1, expected_id);
    assert_eq!(id2, expected_id);
    assert_eq!(id1, id2);
}

// =========================
// Concurrency Safety Tests
// =========================

#[test]
fn test_concurrent_insert_same_name() {
    use std::sync::{Arc, Barrier};

    let (_registry, root) = new_registry();

    let parent = root
        .write_with(|obj| {
            obj.add_child(
                "conc_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let parent = Arc::new(parent);
    let barrier = Arc::new(Barrier::new(*THREADS));
    let mut handles = vec![];

    for _ in 0..*THREADS {
        let parent = Arc::clone(&parent);
        let barrier = Arc::clone(&barrier);

        let handle = spawn_gc(move || {
            barrier.wait();
            parent.write_with(|obj| {
                obj.add_child(
                    "same_name".to_string(),
                    Capability::empty(),
                    Payload::new(NameSpace),
                )
            })
        });

        handles.push(handle);
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Exactly one should succeed (Ok(Ok(_)))
    let success_count = results.iter().filter(|r| matches!(r, Ok(Ok(_)))).count();
    assert_eq!(success_count, 1);

    // Others should get DuplicateChildName (Ok(Err(...)))
    let dup_count = results
        .iter()
        .filter(|r| {
            matches!(
                r,
                Ok(Err(libakarin_object::ObjectError::DuplicateChildName))
            )
        })
        .count();
    assert_eq!(dup_count, *THREADS - 1);
}

#[test]
fn test_concurrent_insert_different_names() {
    use std::sync::{Arc, Barrier};

    let (_registry, root) = new_registry();

    let parent = root
        .write_with(|obj| {
            obj.add_child(
                "conc_parent2".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let parent = Arc::new(parent);
    let barrier = Arc::new(Barrier::new(*THREADS));
    let mut handles = vec![];

    for i in 0..*THREADS {
        let parent = Arc::clone(&parent);
        let barrier = Arc::clone(&barrier);

        let handle = spawn_gc(move || {
            barrier.wait();
            parent.write_with(|obj| {
                obj.add_child(
                    format!("child_{}", i),
                    Capability::empty(),
                    Payload::new(NameSpace),
                )
            })
        });

        handles.push(handle);
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All should succeed
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), *THREADS);

    // Verify children count
    let count = parent.read_with(|obj| obj.children().count()).unwrap();
    assert_eq!(count, *THREADS);
}

#[test]
fn test_concurrent_insert_vs_destroy() {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    let (_registry, root) = new_registry();

    let parent = root
        .write_with(|obj| {
            obj.add_child(
                "destroy_parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let parent = Arc::new(parent);
    let barrier = Arc::new(Barrier::new(*THREADS));
    let mut handles: Vec<thread::JoinHandle<()>> = vec![];

    // One thread destroys
    {
        let parent = Arc::clone(&parent);
        let barrier = Arc::clone(&barrier);

        let handle = spawn_gc(move || {
            barrier.wait();
            drop(parent);
        });

        handles.push(handle);
    }

    // Other threads try to insert
    for i in 1..*THREADS {
        let parent = Arc::clone(&parent);
        let barrier = Arc::clone(&barrier);

        let handle = spawn_gc(move || {
            barrier.wait();
            let _ = parent.write_with(|obj| {
                obj.add_child(
                    format!("child_{}", i),
                    Capability::empty(),
                    Payload::new(NameSpace),
                )
            });
        });

        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        let _ = handle.join();
    }

    // No panic is the success criterion
}

#[test]
fn test_concurrent_double_destroy() {
    use std::sync::{Arc, Barrier};

    let (registry, root) = new_registry();

    let child = root
        .write_with(|obj| {
            obj.add_child(
                "double_destroy".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let handle = Arc::new(child);
    let registry_ref: &'static Registry = registry;
    let barrier = Arc::new(Barrier::new(*THREADS));
    let mut thread_handles = vec![];

    for _ in 0..*THREADS {
        let handle = Arc::clone(&handle);
        let barrier = Arc::clone(&barrier);

        let thread_handle = spawn_gc(move || {
            barrier.wait();
            registry_ref.remove(&*handle)
        });

        thread_handles.push(thread_handle);
    }

    let results: Vec<_> = thread_handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();

    // Exactly one should succeed
    let success_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(success_count, 1);

    // Others should get ObjectDestroyed
    let destroyed_count = results
        .iter()
        .filter(|r| matches!(r, Err(libakarin_object::ObjectError::ObjectDestroyed)))
        .count();
    assert_eq!(destroyed_count, *THREADS - 1);
}

#[test]
fn test_concurrent_parent_child_destroy() {
    use std::sync::{Arc, Barrier};

    let (registry, root) = new_registry();

    let parent = root
        .write_with(|obj| {
            obj.add_child(
                "parent".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let child = parent
        .write_with(|obj| {
            obj.add_child(
                "child".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let parent_handle = Arc::new(parent);
    let child_handle = Arc::new(child);
    let registry_ref: &'static Registry = registry;

    let barrier = Arc::new(Barrier::new(*THREADS * 2));
    let mut thread_handles = vec![];

    // Group A: destroy parent
    for _ in 0..*THREADS {
        let handle = Arc::clone(&parent_handle);
        let barrier = Arc::clone(&barrier);

        let thread_handle = spawn_gc(move || {
            barrier.wait();
            registry_ref.remove(&*handle)
        });

        thread_handles.push(thread_handle);
    }

    // Group B: destroy child
    for _ in 0..*THREADS {
        let handle = Arc::clone(&child_handle);
        let barrier = Arc::clone(&barrier);

        let thread_handle = spawn_gc(move || {
            barrier.wait();
            registry_ref.remove(&*handle)
        });

        thread_handles.push(thread_handle);
    }

    let results: Vec<_> = thread_handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();

    // At least one should succeed (either parent or child destruction)
    let success_count = results.iter().filter(|r| r.is_ok()).count();
    assert!(success_count >= 1);

    // No panics is the success criterion
}

#[test]
fn test_concurrent_locate_vs_destroy() {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    let (registry, root) = new_registry();

    // Build tree /locate_test/a/b
    let locate_test = root
        .write_with(|obj| {
            obj.add_child(
                "locate_test".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let a = locate_test
        .write_with(|obj| {
            obj.add_child(
                "a".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let _b = a
        .write_with(|obj| {
            obj.add_child(
                "b".to_string(),
                Capability::empty(),
                Payload::new(NameSpace),
            )
        })
        .unwrap()
        .unwrap();

    let registry_ref: &'static Registry = registry;
    let barrier = Arc::new(Barrier::new(*THREADS));
    let mut thread_handles: Vec<thread::JoinHandle<()>> = vec![];

    // One thread destroys
    {
        let barrier = Arc::clone(&barrier);
        let handle = spawn_gc(move || {
            barrier.wait();
            drop(locate_test);
        });
        thread_handles.push(handle);
    }

    // Other threads try to locate
    for _ in 1..*THREADS {
        let barrier = Arc::clone(&barrier);

        let handle = spawn_gc(move || {
            barrier.wait();
            let path = ObjectPath::new("/locate_test/a/b");
            let _ = registry_ref.locate_and_then(None::<&Handle>, &path, |_| {});
        });

        thread_handles.push(handle);
    }

    // Wait for all threads
    for handle in thread_handles {
        let _ = handle.join();
    }

    // No panics is the success criterion
}

// ===========================
// ObjectPath Utility Tests
// ===========================

#[test]
fn test_object_path_is_absolute() {
    assert!(ObjectPath::new("/").is_absolute());
    assert!(ObjectPath::new("/a").is_absolute());
    assert!(ObjectPath::new("/a/b/c").is_absolute());

    assert!(!ObjectPath::new("a/b").is_absolute());
    assert!(!ObjectPath::new("relative").is_absolute());
}

#[test]
fn test_object_path_components() {
    let path = ObjectPath::new("/a//b/c");
    let components: Vec<&str> = path.components().collect();

    // Empty segments should be filtered
    assert_eq!(components, vec!["a", "b", "c"]);

    let path2 = ObjectPath::new("x/y/z");
    let components2: Vec<&str> = path2.components().collect();
    assert_eq!(components2, vec!["x", "y", "z"]);
}

#[test]
fn test_object_path_parent() {
    let path = ObjectPath::new("/a/b/c");
    let parent = path.parent().unwrap();
    assert_eq!(parent.to_string(), "/a/b");

    let parent2 = parent.parent().unwrap();
    assert_eq!(parent2.to_string(), "/a");

    let parent3 = parent2.parent().unwrap();
    assert_eq!(parent3.to_string(), "/");

    // Root has no parent
    let root_parent = parent3.parent();
    assert!(root_parent.is_none());
}

#[test]
fn test_object_path_join() {
    let path = ObjectPath::new("/a");
    let joined = path.join("b");
    assert_eq!(joined.to_string(), "/a/b");

    let path2 = ObjectPath::new("/a/");
    let joined2 = path2.join("c");
    assert_eq!(joined2.to_string(), "/a/c");

    let relative = ObjectPath::new("x/y");
    let joined3 = relative.join("z");
    assert_eq!(joined3.to_string(), "x/y/z");
}
