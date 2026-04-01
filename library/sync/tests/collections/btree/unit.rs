use libakarin_sync::collections::btree::BTreeMap as RawConcurrentMap;

use crate::collections::GC;

type ConcurrentMap<K, V, const FANOUT: usize = 64> = RawConcurrentMap<K, V, GC, FANOUT>;

#[test]
fn basic_map() {
    let map = ConcurrentMap::<usize, usize>::default();

    let n = 64; // SPLIT_SIZE
    for i in 0..=n {
        assert_eq!(map.get(&i), None);
        map.insert(i, i);
        assert_eq!(map.get(&i), Some(i), "failed to get key {i}");
    }

    for (i, (k, _v)) in map.range(..).enumerate() {
        assert_eq!(i, k);
    }

    for (i, (k, _v)) in map.range(..).rev().enumerate() {
        assert_eq!(n - i, k);
    }

    for (i, (k, _v)) in map.iter().enumerate() {
        assert_eq!(i, k);
    }

    for (i, (k, _v)) in map.iter().rev().enumerate() {
        assert_eq!(n - i, k);
    }

    for (i, (k, _v)) in map.range(0..).enumerate() {
        assert_eq!(i, k);
    }

    for (i, (k, _v)) in map.range(0..).rev().enumerate() {
        assert_eq!(n - i, k);
    }

    for (i, (k, _v)) in map.range(0..n).enumerate() {
        assert_eq!(i, k);
    }

    for (i, (k, _v)) in map.range(0..n).rev().enumerate() {
        assert_eq!((n - 1) - i, k);
    }

    for (i, (k, _v)) in map.range(0..=n).enumerate() {
        assert_eq!(i, k);
    }

    for (i, (k, _v)) in map.range(0..=n).rev().enumerate() {
        assert_eq!(n - i, k);
    }

    for i in 0..=n {
        assert_eq!(map.get(&i), Some(i), "failed to get key {i}");
    }
}

#[test]
fn timing_map() {
    use std::time::Instant;

    let map = ConcurrentMap::<u64, u64>::default();

    let n = 1024 * 1024;

    let insert = Instant::now();
    for i in 0..n {
        map.insert(i, i);
    }
    let insert_elapsed = insert.elapsed();
    println!(
        "{} inserts/s, total {:?}",
        (n * 1_000_000) / u64::try_from(insert_elapsed.as_micros().max(1)).unwrap_or(u64::MAX),
        insert_elapsed
    );

    let scan = Instant::now();
    let count = map.range(..).count();
    assert_eq!(count as u64, n);
    let scan_elapsed = scan.elapsed();
    println!(
        "{} scanned items/s, total {:?}",
        (n * 1_000_000) / u64::try_from(scan_elapsed.as_micros().max(1)).unwrap_or(u64::MAX),
        scan_elapsed
    );

    let scan_rev = Instant::now();
    let count = map.range(..).rev().count();
    assert_eq!(count as u64, n);
    let scan_rev_elapsed = scan_rev.elapsed();
    println!(
        "{} reverse-scanned items/s, total {:?}",
        (n * 1_000_000) / u64::try_from(scan_rev_elapsed.as_micros().max(1)).unwrap_or(u64::MAX),
        scan_rev_elapsed
    );

    let gets = Instant::now();
    for i in 0..n {
        map.get(&i);
    }
    let gets_elapsed = gets.elapsed();
    println!(
        "{} gets/s, total {:?}",
        (n * 1_000_000) / u64::try_from(gets_elapsed.as_micros().max(1)).unwrap_or(u64::MAX),
        gets_elapsed
    );
}
