use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use lazy_static::lazy_static;

lazy_static! {
    static ref CPU_NUM: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
}

fn enter_leave(c: &mut Criterion) {
    let mut group = c.benchmark_group("enter_leave");
    group.bench_function("libakarin_sync::gc", |b| {
        let collector = libakarin_sync::gc::Collector::new(*CPU_NUM);
        let handle = collector.register();
        b.iter(|| {
            black_box(handle.pin());
        });
    });

    group.bench_function("crossbeam", |b| {
        b.iter(|| {
            black_box(crossbeam_epoch::pin());
        });
    });
}

criterion_group!(benches, enter_leave);
criterion_main!(benches);
