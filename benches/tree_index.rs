use scc::ebr::Barrier;
use scc::TreeIndex;

use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion};

fn insert(c: &mut Criterion) {
    c.bench_function("TreeIndex: insert", |b| {
        b.iter_custom(|iters| {
            let treeindex: TreeIndex<u64, u64> = TreeIndex::default();
            let start = Instant::now();
            for i in 0..iters {
                assert!(treeindex.insert(i, i).is_ok());
            }
            start.elapsed()
        })
    });
}

fn insert_rev(c: &mut Criterion) {
    c.bench_function("TreeIndex: insert, rev", |b| {
        b.iter_custom(|iters| {
            let treeindex: TreeIndex<u64, u64> = TreeIndex::default();
            let start = Instant::now();
            for i in (0..iters).rev() {
                assert!(treeindex.insert(i, i).is_ok());
            }
            start.elapsed()
        })
    });
}

fn iter_with(c: &mut Criterion) {
    c.bench_function("TreeIndex: iter_with", |b| {
        b.iter_custom(|iters| {
            let treeindex: TreeIndex<u64, u64> = TreeIndex::default();
            for i in 0..iters {
                assert!(treeindex.insert(i, i).is_ok());
            }
            let start = Instant::now();
            let barrier = Barrier::new();
            let iter = treeindex.iter(&barrier);
            for e in iter {
                assert_eq!(e.0, e.1);
            }
            start.elapsed()
        })
    });
}

fn read_with(c: &mut Criterion) {
    c.bench_function("TreeIndex: read_with", |b| {
        b.iter_custom(|iters| {
            let treeindex: TreeIndex<u64, u64> = TreeIndex::default();
            for i in 0..iters {
                assert!(treeindex.insert(i, i).is_ok());
            }
            let start = Instant::now();
            let barrier = Barrier::new();
            for i in 0..iters {
                assert_eq!(
                    treeindex.read_with(&i, |_, v| *v == i, &barrier),
                    Some(true)
                );
            }
            start.elapsed()
        })
    });
}

criterion_group!(tree_index, insert, insert_rev, iter_with, read_with);
criterion_main!(tree_index);
