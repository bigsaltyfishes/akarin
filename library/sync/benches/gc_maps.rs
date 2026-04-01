use std::{
    hint::black_box,
    sync::{Arc, Barrier},
    thread,
};

use criterion::{Criterion, criterion_group, criterion_main};
use crossbeam_skiplist::SkipMap as CrossbeamSkipMap;
use lazy_static::lazy_static;
use libakarin_sync::{
    collections::{btree::BTreeMap as RawBTreeMap, skiplist::SkipMap as RawSkipMap},
    gc::{Collector, GarbageCollector, Guard, LocalHandle},
};

type SkipMap<K, V> = RawSkipMap<K, V, GC>;
type BTreeMap<K, V> = RawBTreeMap<K, V, GC>;

const THREADS: usize = 16;
const ITEMS: usize = 1000;

lazy_static! {
    static ref CPU_NUM: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
}

lazy_static! {
    static ref COLLECTOR: Collector = Collector::new(*CPU_NUM).batch_size(32);
}

thread_local! {
    static LOCAL_HANDLE: LocalHandle = COLLECTOR.register();
}

struct GC;

impl GarbageCollector for GC {
    fn global_handle() -> Collector {
        COLLECTOR.clone()
    }

    fn local_handle() -> LocalHandle {
        LOCAL_HANDLE.with(|handle| handle.clone())
    }

    fn pin() -> Guard {
        LOCAL_HANDLE.with(|handle| handle.pin())
    }
}

// ============================================================================
// Single-threaded benchmarks
// ============================================================================

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_insert");

    group.bench_function("crossbeam-skiplist", |b| {
        b.iter(|| {
            let map = CrossbeamSkipMap::new();
            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                map.insert(num, !num);
            }
            black_box(&map);
        });
    });

    group.bench_function("libakarin_sync/skiplist", |b| {
        b.iter(|| {
            let map = SkipMap::new();
            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                map.insert(num, !num);
            }
            black_box(&map);
        });
    });

    group.bench_function("libakarin_sync/btreemap", |b| {
        b.iter(|| {
            let map = BTreeMap::new();
            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                map.insert(num, !num);
            }
            black_box(&map);
        });
    });

    group.finish();
}

fn bench_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_lookup");

    // Setup crossbeam map
    let crossbeam_map = CrossbeamSkipMap::new();
    let mut num = 0u64;
    for _ in 0..ITEMS {
        num = num.wrapping_mul(17).wrapping_add(255);
        crossbeam_map.insert(num, !num);
    }

    // Setup akarin skipmap
    let akarin_skipmap = SkipMap::new();
    let mut num = 0u64;
    for _ in 0..ITEMS {
        num = num.wrapping_mul(17).wrapping_add(255);
        akarin_skipmap.insert(num, !num);
    }

    // Setup akarin btreemap
    let akarin_btreemap = BTreeMap::new();
    let mut num = 0u64;
    for _ in 0..ITEMS {
        num = num.wrapping_mul(17).wrapping_add(255);
        akarin_btreemap.insert(num, !num);
    }

    group.bench_function("crossbeam-skiplist", |b| {
        b.iter(|| {
            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                black_box(crossbeam_map.get(&num));
            }
        });
    });

    group.bench_function("libakarin_sync/skiplist", |b| {
        b.iter(|| {
            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                black_box(akarin_skipmap.get(&num));
            }
        });
    });

    group.bench_function("libakarin_sync/btreemap", |b| {
        b.iter(|| {
            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                black_box(akarin_btreemap.get(&num));
            }
        });
    });

    group.finish();
}

fn bench_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_iter");

    // Setup crossbeam map
    let crossbeam_map = CrossbeamSkipMap::new();
    let mut num = 0u64;
    for _ in 0..ITEMS {
        num = num.wrapping_mul(17).wrapping_add(255);
        crossbeam_map.insert(num, !num);
    }

    // Setup akarin skipmap
    let akarin_skipmap = SkipMap::new();
    let mut num = 0u64;
    for _ in 0..ITEMS {
        num = num.wrapping_mul(17).wrapping_add(255);
        akarin_skipmap.insert(num, !num);
    }

    // Setup akarin btreemap
    let akarin_btreemap = BTreeMap::new();
    let mut num = 0u64;
    for _ in 0..ITEMS {
        num = num.wrapping_mul(17).wrapping_add(255);
        akarin_btreemap.insert(num, !num);
    }

    group.bench_function("crossbeam-skiplist", |b| {
        b.iter(|| {
            for entry in crossbeam_map.iter() {
                black_box(entry);
            }
        });
    });

    group.bench_function("libakarin_sync/skiplist", |b| {
        b.iter(|| {
            for entry in akarin_skipmap.iter() {
                black_box(entry);
            }
        });
    });

    group.bench_function("libakarin_sync/btreemap", |b| {
        b.iter(|| {
            for entry in akarin_btreemap.iter() {
                black_box(entry);
            }
        });
    });

    group.finish();
}

fn bench_insert_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_insert_remove");

    group.bench_function("crossbeam-skiplist", |b| {
        b.iter(|| {
            let map = CrossbeamSkipMap::new();

            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                map.insert(num, !num);
            }

            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                black_box(map.remove(&num));
            }
        });
    });

    group.bench_function("libakarin_sync/skiplist", |b| {
        b.iter(|| {
            let map = SkipMap::new();

            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                map.insert(num, !num);
            }

            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                black_box(map.remove(&num));
            }
        });
    });

    group.bench_function("libakarin_sync/btreemap", |b| {
        b.iter(|| {
            let map = BTreeMap::new();

            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                map.insert(num, !num);
            }

            let mut num = 0u64;
            for _ in 0..ITEMS {
                num = num.wrapping_mul(17).wrapping_add(255);
                black_box(map.remove(&num));
            }
        });
    });

    group.finish();
}

// ============================================================================
// Multi-threaded benchmarks
// ============================================================================

fn bench_concurrent_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_concurrent_insert");

    group.bench_function("crossbeam-skiplist", |b| {
        b.iter(|| {
            let map = Arc::new(CrossbeamSkipMap::new());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let map = map.clone();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        let mut num = t as u64;
                        for _ in 0..ITEMS {
                            num = num.wrapping_mul(17).wrapping_add(255);
                            map.insert(num, !num);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
            black_box(&map);
        });
    });

    group.bench_function("libakarin_sync/skiplist", |b| {
        b.iter(|| {
            let map = Arc::new(SkipMap::new());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let map = map.clone();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        let mut num = t as u64;
                        for _ in 0..ITEMS {
                            num = num.wrapping_mul(17).wrapping_add(255);
                            map.insert(num, !num);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
            black_box(&map);
        });
    });

    group.bench_function("libakarin_sync/btreemap", |b| {
        b.iter(|| {
            let map = Arc::new(BTreeMap::new());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let map = map.clone();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        let mut num = t as u64;
                        for _ in 0..ITEMS {
                            num = num.wrapping_mul(17).wrapping_add(255);
                            map.insert(num, !num);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
            black_box(&map);
        });
    });

    group.finish();
}

fn bench_concurrent_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_concurrent_mixed");

    group.bench_function("crossbeam-skiplist", |b| {
        b.iter(|| {
            let map = Arc::new(CrossbeamSkipMap::new());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let map = map.clone();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        let mut num = t as u64;
                        for _ in 0..ITEMS {
                            num = num.wrapping_mul(17).wrapping_add(255);
                            map.insert(num, !num);
                            black_box(map.get(&num));
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    group.bench_function("libakarin_sync/skiplist", |b| {
        b.iter(|| {
            let map = Arc::new(SkipMap::new());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let map = map.clone();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        let mut num = t as u64;
                        for _ in 0..ITEMS {
                            num = num.wrapping_mul(17).wrapping_add(255);
                            map.insert(num, !num);
                            black_box(map.get(&num));
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    group.bench_function("libakarin_sync/btreemap", |b| {
        b.iter(|| {
            let map = Arc::new(BTreeMap::new());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let map = map.clone();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        let mut num = t as u64;
                        for _ in 0..ITEMS {
                            num = num.wrapping_mul(17).wrapping_add(255);
                            map.insert(num, !num);
                            black_box(map.get(&num));
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_lookup,
    bench_iter,
    bench_insert_remove,
    bench_concurrent_insert,
    bench_concurrent_mixed
);
criterion_main!(benches);
