//! Statistics accumulators (M3-02, spec §7.2).

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use tachy_core::{
    stats::{Accumulate, ColumnStats, Hll, Scalar, SpaceSaving, StatsMode, StatsSource, TopK},
    types::{ColType, NullSet, parse_value},
};

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[test]
fn hll_within_two_percent_at_one_million_over_ten_seeds() {
    let n = 1_000_000u64;
    for seed in 1..=10u64 {
        let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mask = xorshift(&mut s);
        let mut h = Hll::new();
        for i in 0..n {
            // XOR with a constant is a bijection: exactly n distinct values.
            h.insert(&(i ^ mask).to_le_bytes());
        }
        let est = h.estimate() as f64;
        let err = (est - n as f64).abs() / n as f64;
        assert!(err < 0.02, "seed {seed}: estimate {est}, error {err:.4}");
    }
}

#[test]
fn hll_exact_below_1024() {
    for n in [0usize, 1, 10, 500, 1023, 1024] {
        let mut h = Hll::new();
        for i in 0..n {
            h.insert(format!("value {i}").as_bytes());
            h.insert(format!("value {i}").as_bytes());
        }
        assert_eq!(h.estimate(), n as u64);
    }
}

fn stats_of(ty: ColType, mode: StatsMode, values: &[&str]) -> ColumnStats {
    let nulls = NullSet::default();
    let mut s = ColumnStats::new(ty, mode);
    for v in values {
        let parsed = parse_value(ty, v.as_bytes(), &nulls);
        s.push(v.as_bytes(), &parsed);
    }
    s
}

#[test]
fn top5_plus_other_is_the_non_null_count() {
    let mut values = Vec::new();
    for i in 0..500 {
        values.push(["a", "b", "c", "d", "e", "f", "g", "", "NA"][i % 9]);
        if i % 4 == 0 {
            values.push("rare");
        }
    }
    for ty in [ColType::Str, ColType::Enum, ColType::I64] {
        let s = stats_of(ty, StatsMode::Sample, &values);
        let top: u64 = s.top5().iter().map(|e| e.count).sum();
        assert_eq!(top + s.other(), s.rows_seen - s.nulls);
        assert_eq!(s.top5().len(), 5);
    }
    // Fewer than five distinct values: other = 0.
    let s = stats_of(ColType::Str, StatsMode::Sample, &["x", "y", "x", ""]);
    assert_eq!(s.top5().len(), 2);
    assert_eq!(s.other(), 0);
    assert_eq!(s.non_null(), 3);
}

#[test]
fn numeric_stats_on_known_fixture() {
    let owned: Vec<String> = (1..=100).rev().map(|i| i.to_string()).collect();
    let mut values: Vec<&str> = owned.iter().map(String::as_str).collect();
    values.extend(["", "NULL", "n/a"]);
    let s = stats_of(ColType::I64, StatsMode::Sample, &values);
    let n = s.numeric.as_ref().unwrap();
    assert_eq!(n.min(), Some(Scalar::I64(1)));
    assert_eq!(n.max(), Some(Scalar::I64(100)));
    assert_eq!(n.mean(), Some(50.5));
    assert_eq!(n.p50(), Some(50.5));
    assert!((n.p95().unwrap() - 95.05).abs() < 1e-9);
    assert!(!n.quantiles_approximate());
    assert_eq!(s.nulls, 2);
    assert_eq!(s.rows_seen, 103);
    assert_eq!(
        s.null_percent().map(|p| (p * 10.0).round() / 10.0),
        Some(1.9)
    );
    assert_eq!(s.distinct.estimate(), 101); // "n/a" is an invalid but distinct value
    assert_eq!(s.max_width, 3);
    assert_eq!(s.label(), "sample 103 rows");

    let f = stats_of(
        ColType::F64,
        StatsMode::Sample,
        &["0.5", "-1.25", "2", "1e2"],
    );
    let n = f.numeric.as_ref().unwrap();
    assert_eq!(n.min(), Some(Scalar::F64(-1.25)));
    assert_eq!(n.max(), Some(Scalar::F64(100.0)));
    assert_eq!(n.mean(), Some(101.25 / 4.0));
    assert_eq!(n.p50(), Some(1.25));
    assert!(
        stats_of(ColType::Str, StatsMode::Sample, &["1"])
            .numeric
            .is_none()
    );
}

fn sample_values(n: usize, seed: u64) -> Vec<String> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| match xorshift(&mut s) % 20 {
            0 => String::new(),
            1 => "oops".to_owned(),
            r => ((xorshift(&mut s) % 3000) as i64 - 1000 + r as i64).to_string(),
        })
        .collect()
}

#[test]
fn sample_merge_of_halves_equals_whole() {
    let owned = sample_values(20_000, 42);
    let values: Vec<&str> = owned.iter().map(String::as_str).collect();
    let whole = stats_of(ColType::I64, StatsMode::Sample, &values);
    let mut a = stats_of(ColType::I64, StatsMode::Sample, &values[..7_000]);
    let b = stats_of(ColType::I64, StatsMode::Sample, &values[7_000..]);
    a.merge(b);
    assert_eq!(a.rows_seen, whole.rows_seen);
    assert_eq!(a.nulls, whole.nulls);
    assert_eq!(a.max_width, whole.max_width);
    assert_eq!(a.distinct, whole.distinct);
    assert_eq!(a.top5(), whole.top5());
    assert_eq!(a.other(), whole.other());
    assert_eq!(a.source, StatsSource::Sample { rows: 20_000 });
    assert_eq!(a.label(), "sample 20k rows");
    let (na, nw) = (a.numeric.unwrap(), whole.numeric.unwrap());
    assert_eq!(na.min(), nw.min());
    assert_eq!(na.max(), nw.max());
    assert_eq!(na.mean(), nw.mean());
    assert_eq!(na.p50(), nw.p50());
    assert_eq!(na.p95(), nw.p95());
}

#[test]
fn full_profile_merge_within_documented_error() {
    let owned = sample_values(100_000, 7);
    let values: Vec<&str> = owned.iter().map(String::as_str).collect();
    let whole = stats_of(ColType::I64, StatsMode::Full, &values);
    let mut parts = values
        .chunks(13_000)
        .map(|c| stats_of(ColType::I64, StatsMode::Full, c));
    let mut merged = parts.next().unwrap();
    for p in parts {
        merged.merge(p);
    }
    assert_eq!(merged.source, StatsSource::AllRows);
    assert_eq!(merged.label(), "all rows");
    assert_eq!(merged.rows_seen, whole.rows_seen);
    assert_eq!(merged.nulls, whole.nulls);
    // HLL registers merge exactly.
    assert_eq!(merged.distinct, whole.distinct);
    // Exact parts.
    let (nm, nw) = (
        merged.numeric.as_ref().unwrap(),
        whole.numeric.as_ref().unwrap(),
    );
    assert_eq!(nm.min(), nw.min());
    assert_eq!(nm.max(), nw.max());
    assert_eq!(nm.mean(), nw.mean());
    // KLL: within the rank error, against exact ranks.
    let mut sorted: Vec<f64> = values
        .iter()
        .filter_map(|v| v.parse::<f64>().ok())
        .collect();
    sorted.sort_by(f64::total_cmp);
    for (q, got) in [(0.5, nm.p50().unwrap()), (0.95, nm.p95().unwrap())] {
        assert!(nm.quantiles_approximate());
        let rank = sorted.partition_point(|&x| x < got) as f64 / sorted.len() as f64;
        assert!((rank - q).abs() < 0.02, "q={q} got={got} rank={rank}");
    }
    // Space-Saving: ~2,000 distinct values exceed the 1,024 counters, so the
    // counts are bounds.
    let mut truth: HashMap<&str, u64> = HashMap::new();
    for v in values.iter().filter(|v| !v.is_empty()) {
        *truth.entry(v).or_insert(0) += 1;
    }
    for e in merged.top5() {
        let f = truth[std::str::from_utf8(&e.value).unwrap()];
        assert!(e.count - e.max_overcount <= f && f <= e.count);
    }
}

#[test]
fn recompute_after_set_type_uses_cached_values_only() {
    // The end-to-end acceptance test (no file access after the sample) is
    // `tests/sample.rs::recompute_after_set_type_needs_no_file_access`. This
    // checks the accumulator part: stats rebuilt from cached bytes.
    let cached: Vec<Vec<u8>> = ["1", "2", "x", "", "3"]
        .iter()
        .map(|v| v.as_bytes().to_vec())
        .collect();
    let nulls = NullSet::default();
    let as_str = ColumnStats::from_values(ColType::Str, cached.iter().map(Vec::as_slice), &nulls);
    assert!(as_str.numeric.is_none());
    let as_int = ColumnStats::from_values(ColType::I64, cached.iter().map(Vec::as_slice), &nulls);
    assert!(as_str.is_stale_for(ColType::I64));
    assert!(!as_int.is_stale_for(ColType::I64));
    assert_eq!(as_int.numeric.as_ref().unwrap().max(), Some(Scalar::I64(3)));
    assert_eq!(as_int.rows_seen, 5);
}

// --- merge properties ---

fn hll_of(values: &[u32]) -> Hll {
    let mut h = Hll::new();
    for v in values {
        h.insert(&v.to_le_bytes());
    }
    h
}

fn ss_of(values: &[u8], cap: usize) -> SpaceSaving {
    let mut s = SpaceSaving::with_capacity(cap);
    for v in values {
        s.insert(&[*v]);
    }
    s
}

fn merged_ss(a: &SpaceSaving, b: &SpaceSaving) -> SpaceSaving {
    let mut x = TopK::SpaceSaving(a.clone());
    x.merge(TopK::SpaceSaving(b.clone()));
    match x {
        TopK::SpaceSaving(s) => s,
        TopK::Exact(_) => unreachable!(),
    }
}

/// The Space-Saving guarantees against the true counts.
fn check_bounds(s: &SpaceSaving, data: &[&[u8]]) -> Result<(), TestCaseError> {
    let mut truth: HashMap<u8, u64> = HashMap::new();
    for part in data {
        for v in *part {
            *truth.entry(*v).or_insert(0) += 1;
        }
    }
    let min = s.min_count();
    for (v, &f) in &truth {
        match s.get(&[*v]) {
            Some((c, e)) => prop_assert!(c - e <= f && f <= c, "{v}: {c}-{e} vs {f}"),
            None => prop_assert!(f <= min, "{v}: {f} > {min}"),
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn hll_merge_is_associative_and_commutative(
        a in prop::collection::vec(any::<u32>(), 0..1500),
        b in prop::collection::vec(0u32..3000, 0..1500),
        c in prop::collection::vec(any::<u32>(), 0..600),
    ) {
        let (ha, hb, hc) = (hll_of(&a), hll_of(&b), hll_of(&c));
        let mut ab = ha.clone();
        ab.merge(hb.clone());
        let mut ba = hb.clone();
        ba.merge(ha.clone());
        prop_assert_eq!(&ab, &ba);

        let mut ab_c = ab.clone();
        ab_c.merge(hc.clone());
        let mut bc = hb.clone();
        bc.merge(hc.clone());
        let mut a_bc = ha.clone();
        a_bc.merge(bc);
        prop_assert_eq!(&ab_c, &a_bc);

        // Merging equals inserting everything into one sketch.
        let all: Vec<u32> = a.iter().chain(&b).chain(&c).copied().collect();
        prop_assert_eq!(&ab_c, &hll_of(&all));
        let distinct = all.iter().collect::<HashSet<_>>().len();
        if distinct <= 1024 {
            prop_assert_eq!(ab_c.estimate(), distinct as u64);
        }
    }

    #[test]
    fn space_saving_merge_is_commutative_and_associative_within_bounds(
        a in prop::collection::vec(0u8..40, 0..300),
        b in prop::collection::vec(0u8..40, 0..300),
        c in prop::collection::vec(0u8..40, 0..300),
        cap in 4usize..48,
    ) {
        let (sa, sb, sc) = (ss_of(&a, cap), ss_of(&b, cap), ss_of(&c, cap));
        prop_assert_eq!(merged_ss(&sa, &sb), merged_ss(&sb, &sa));

        let ab_c = merged_ss(&merged_ss(&sa, &sb), &sc);
        let a_bc = merged_ss(&sa, &merged_ss(&sb, &sc));
        let data: [&[u8]; 3] = [&a, &b, &c];
        check_bounds(&ab_c, &data)?;
        check_bounds(&a_bc, &data)?;
        // With enough counters both groupings are exact and identical.
        if cap >= 40 {
            prop_assert_eq!(ab_c, a_bc);
        }
    }
}
