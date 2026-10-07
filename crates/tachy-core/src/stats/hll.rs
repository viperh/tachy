//! HyperLogLog distinct-count estimator (spec §7.2), written by hand.
//!
//! - Precision `p = 14`: 16,384 one-byte registers (16 KiB), standard error
//!   about 0.81 %.
//! - Hash: [`wyhash`], a 64-bit wyhash-style function with a fixed seed, so
//!   estimates are stable across runs, threads and merges. (std's SipHash with
//!   fixed keys would also work, but is several times slower per value.)
//! - **Sparse mode:** up to [`SPARSE_LIMIT`] distinct hashes are kept exactly in
//!   a set, so small counts are exact. Past that the set is converted to
//!   registers.
//! - Small-range correction: linear counting while the raw estimate is below
//!   `2.5 m` and some registers are still zero.
//! - [`Hll::merge`]: register-wise max (sparse sets: union, converting when the
//!   union grows too large). Merging is associative and commutative.

use std::collections::HashSet;

use super::Accumulate;
use crate::types::Value;

/// Register-index bits.
pub const PRECISION: u32 = 14;
/// Number of registers (`2^p`).
pub const REGISTERS: usize = 1 << PRECISION;
/// Most distinct hashes kept exactly before switching to registers.
pub const SPARSE_LIMIT: usize = 1024;

const SEED: u64 = 0x7461_6368_795f_686c; // "tachy_hl"

const P0: u64 = 0xa076_1d64_78bd_642f;
const P1: u64 = 0xe703_7ed1_a0b4_28db;
const P2: u64 = 0x8ebc_6af0_9c88_c6e3;
const P3: u64 = 0x5899_65cc_7537_4cc3;

#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

#[inline]
fn r8(d: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[i..i + 8]);
    u64::from_le_bytes(b)
}

#[inline]
fn r4(d: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&d[i..i + 4]);
    u64::from(u32::from_le_bytes(b))
}

/// A 64-bit wyhash-style hash of `data` with a fixed seed. Stable across runs
/// and platforms.
pub fn wyhash(data: &[u8]) -> u64 {
    let len = data.len();
    let mut seed = SEED ^ mum(SEED ^ P0, P1);
    let (a, b);
    if len <= 16 {
        if len >= 4 {
            let q = (len >> 3) << 2;
            a = (r4(data, 0) << 32) | r4(data, q);
            b = (r4(data, len - 4) << 32) | r4(data, len - 4 - q);
        } else if len > 0 {
            a = (u64::from(data[0]) << 16)
                | (u64::from(data[len >> 1]) << 8)
                | u64::from(data[len - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = len;
        let mut p = 0;
        if i > 48 {
            let (mut s1, mut s2) = (seed, seed);
            while i > 48 {
                seed = mum(r8(data, p) ^ P1, r8(data, p + 8) ^ seed);
                s1 = mum(r8(data, p + 16) ^ P2, r8(data, p + 24) ^ s1);
                s2 = mum(r8(data, p + 32) ^ P3, r8(data, p + 40) ^ s2);
                p += 48;
                i -= 48;
            }
            seed ^= s1 ^ s2;
        }
        while i > 16 {
            seed = mum(r8(data, p) ^ P1, r8(data, p + 8) ^ seed);
            p += 16;
            i -= 16;
        }
        a = r8(data, len - 16);
        b = r8(data, len - 8);
    }
    mum(P1 ^ len as u64, mum(a ^ P1, b ^ seed))
}

/// A HyperLogLog sketch (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hll {
    /// Exact set of value hashes, at most [`SPARSE_LIMIT`] entries.
    Sparse(HashSet<u64>),
    /// `2^p` registers.
    Dense(Box<[u8]>),
}

impl Default for Hll {
    fn default() -> Hll {
        Hll::new()
    }
}

#[inline]
fn register_update(regs: &mut [u8], hash: u64) {
    let idx = (hash >> (64 - PRECISION)) as usize;
    // The guard bit caps the rank at 64 - p + 1.
    let rest = (hash << PRECISION) | (1 << (PRECISION - 1));
    let rank = rest.leading_zeros() as u8 + 1;
    if regs[idx] < rank {
        regs[idx] = rank;
    }
}

fn densify(set: &HashSet<u64>) -> Box<[u8]> {
    let mut regs = vec![0u8; REGISTERS].into_boxed_slice();
    for &h in set {
        register_update(&mut regs, h);
    }
    regs
}

impl Hll {
    /// An empty sketch.
    pub fn new() -> Hll {
        Hll::Sparse(HashSet::new())
    }

    /// Adds a value (raw bytes).
    pub fn insert(&mut self, v: &[u8]) {
        self.insert_hash(wyhash(v));
    }

    /// Adds a precomputed [`wyhash`].
    pub fn insert_hash(&mut self, hash: u64) {
        match self {
            Hll::Sparse(set) => {
                if set.insert(hash) && set.len() > SPARSE_LIMIT {
                    *self = Hll::Dense(densify(set));
                }
            }
            Hll::Dense(regs) => register_update(regs, hash),
        }
    }

    /// Whether the estimate is exact (sparse mode, up to 1,024 values).
    /// Hash collisions aside, which are negligible at this size.
    pub fn is_exact(&self) -> bool {
        matches!(self, Hll::Sparse(_))
    }

    /// The estimated number of distinct values.
    pub fn estimate(&self) -> u64 {
        match self {
            Hll::Sparse(set) => set.len() as u64,
            Hll::Dense(regs) => {
                let m = REGISTERS as f64;
                let mut sum = 0.0;
                let mut zeros = 0usize;
                for &r in regs.iter() {
                    sum += f64::powi(2.0, -i32::from(r));
                    zeros += usize::from(r == 0);
                }
                let alpha = 0.7213 / (1.0 + 1.079 / m);
                let raw = alpha * m * m / sum;
                let est = if raw <= 2.5 * m && zeros > 0 {
                    m * (m / zeros as f64).ln()
                } else {
                    raw
                };
                est.round() as u64
            }
        }
    }
}

impl Accumulate for Hll {
    /// Counts non-null values only, hashed on their raw bytes.
    fn push(&mut self, v: &[u8], parsed: &Value) {
        if !matches!(parsed, Value::Null) {
            self.insert(v);
        }
    }

    fn merge(&mut self, other: Hll) {
        match (&mut *self, other) {
            (Hll::Sparse(a), Hll::Sparse(b)) => {
                a.extend(b);
                if a.len() > SPARSE_LIMIT {
                    *self = Hll::Dense(densify(a));
                }
            }
            (Hll::Sparse(a), Hll::Dense(mut regs)) => {
                for &h in a.iter() {
                    register_update(&mut regs, h);
                }
                *self = Hll::Dense(regs);
            }
            (Hll::Dense(regs), Hll::Sparse(b)) => {
                for h in b {
                    register_update(regs, h);
                }
            }
            (Hll::Dense(a), Hll::Dense(b)) => {
                for (x, y) in a.iter_mut().zip(b.iter()) {
                    *x = (*x).max(*y);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_spreads() {
        // Fixed seed: the same input hashes the same way every time.
        assert_eq!(wyhash(b"tachy"), wyhash(b"tachy"));
        let lens: Vec<u64> = (0..100).map(|n| wyhash(&vec![b'a'; n])).collect();
        let distinct: HashSet<_> = lens.iter().collect();
        assert_eq!(distinct.len(), 100);
        assert_ne!(wyhash(b""), wyhash(b"\0"));
    }

    #[test]
    fn exact_below_sparse_limit() {
        let mut h = Hll::new();
        for i in 0..SPARSE_LIMIT {
            h.insert(i.to_string().as_bytes());
            h.insert(i.to_string().as_bytes());
        }
        assert!(h.is_exact());
        assert_eq!(h.estimate(), SPARSE_LIMIT as u64);
        h.insert(b"one more");
        assert!(!h.is_exact());
        let e = h.estimate() as f64;
        assert!((e - 1025.0).abs() / 1025.0 < 0.03, "{e}");
    }

    #[test]
    fn nulls_are_not_counted() {
        let mut h = Hll::new();
        h.push(b"", &Value::Null);
        h.push(b"a", &Value::Bytes(b"a"));
        h.push(b"x", &Value::Invalid);
        assert_eq!(h.estimate(), 2);
    }

    #[test]
    fn merge_of_halves_equals_whole() {
        let mut whole = Hll::new();
        let mut a = Hll::new();
        let mut b = Hll::new();
        for i in 0..50_000u32 {
            let v = i.to_le_bytes();
            whole.insert(&v);
            if i % 3 == 0 {
                a.insert(&v)
            } else {
                b.insert(&v)
            }
        }
        a.merge(b);
        assert_eq!(a, whole);
    }
}
