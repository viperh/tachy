//! Sniffer speed (M1-02): a 64 KiB sample must take < 5 ms.
//!
//! `cargo bench -p tachy-core --bench sniff`

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tachy_core::dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff};

fn sample(quoted_newlines: bool) -> Vec<u8> {
    let mut text = b"id,name,price,when,flag,note\n".to_vec();
    let mut i = 0u64;
    while text.len() < 2 * DEFAULT_SAMPLE_BYTES {
        let note = if quoted_newlines && i.is_multiple_of(10) {
            "\"two\nlines\"".to_string()
        } else {
            format!("note {i}")
        };
        text.extend_from_slice(
            format!(
                "{i},\"name {i}, quoted\",{}.25,2024-01-{:02},true,{note}\n",
                i * 3,
                i % 28 + 1
            )
            .as_bytes(),
        );
        i += 1;
    }
    text
}

fn bench_sniff(c: &mut Criterion) {
    let mut group = c.benchmark_group("sniff");
    group.throughput(Throughput::Bytes(DEFAULT_SAMPLE_BYTES as u64));
    for (name, quoted) in [("64k", false), ("64k_quoted_newlines", true)] {
        let text = sample(quoted);
        group.bench_function(name, |b| {
            b.iter(|| {
                sniff(
                    black_box(&text),
                    DEFAULT_SAMPLE_BYTES,
                    &DialectOverrides::default(),
                )
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sniff);
criterion_main!(benches);
