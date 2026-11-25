//! Experimental exact top-k with a conservative quantization error bound.
//! This remains a vector-only linear scan, not a production ANN integration.
use super::{check_cancel, encode, Code, Hit, QuantizedIndex, DIM, MAX_SHORTLIST};
use anyhow::{ensure, Result};
use std::{cmp::Reverse, collections::BinaryHeap, sync::atomic::AtomicBool};

struct Bounds {
    original_norm: f64,
    code_norm: f64,
    residual_norm: f64,
}

impl Bounds {
    fn new(vector: &[f32; DIM], code: &Code) -> Self {
        let (mut original, mut reconstructed, mut residual) = (0.0, 0.0, 0.0);
        for (&value, &quantized) in vector.iter().zip(&code.values) {
            let value = f64::from(value);
            let quantized = f64::from(quantized) * f64::from(code.scale);
            original += value * value;
            reconstructed += quantized * quantized;
            residual += (value - quantized).powi(2);
        }
        // Round norm estimates upward with slack well above binary64 rounding
        // error for 512 bounded components (including subnormal f32 inputs).
        let upper = |s: f64| s.sqrt() * (1.0 + 1e-12) + 1e-12;
        Self {
            original_norm: upper(original),
            code_norm: upper(reconstructed),
            residual_norm: upper(residual),
        }
    }

    fn upper_score(&self, code: &Code, query: &Code, q: &Self) -> f64 {
        // 512 * 127^2 fits in i32; reconstruct the dot product in binary64.
        let dot: i32 = code
            .values
            .iter()
            .zip(&query.values)
            .map(|(&a, &b)| i32::from(a) * i32::from(b))
            .sum();
        let estimate = f64::from(dot) * f64::from(code.scale) * f64::from(query.scale);
        // x·y - xq·yq = (x-xq)·y + xq·(y-yq). Cauchy-Schwarz
        // bounds the two residuals. For the original f32 dot product, the
        // standard gamma_513 bound is < 4e-5 times sum(|x_i*y_i|), itself
        // <= ||x||*||y||. The absolute slack also covers underflow and f64
        // arithmetic error at the validated component magnitude (<= 1.01).
        estimate
            + self.residual_norm * q.original_norm
            + self.code_norm * q.residual_norm
            + 4e-5 * self.original_norm * q.original_norm
            + 1e-8
    }
}

pub struct ExactSearch {
    pub hits: Vec<Hit>,
    pub scored: usize,
    pub pruned: usize,
}

/// Borrows immutable original vectors and their quantized codes together.
/// Every candidate that cannot be ruled out is scored by the legacy f32 dot
/// product. A tie at the bound is never pruned. Cancellation returns an error.
pub struct ExactPruner<'a, 'data> {
    index: &'a QuantizedIndex<'data>,
    bounds: Vec<Bounds>,
}

impl<'a, 'data> ExactPruner<'a, 'data> {
    pub fn build(index: &'a QuantizedIndex<'data>, cancel: &AtomicBool) -> Result<Self> {
        check_cancel(cancel)?;
        let mut bounds = Vec::new();
        bounds.try_reserve_exact(index.original.len())?;
        for (id, (original, code)) in index.original.iter().zip(&index.codes).enumerate() {
            if id.is_multiple_of(128) {
                check_cancel(cancel)?;
            }
            bounds.push(Bounds::new(original, code));
        }
        check_cancel(cancel)?;
        Ok(Self { index, bounds })
    }

    pub fn allocated_bytes(&self) -> usize {
        self.bounds.capacity() * std::mem::size_of::<Bounds>()
    }

    pub fn search(&self, query: &[f32; DIM], k: usize, cancel: &AtomicBool) -> Result<ExactSearch> {
        ensure!(
            (1..=MAX_SHORTLIST).contains(&k),
            "invalid exact result count"
        );
        check_cancel(cancel)?;
        let q = encode(query)?;
        let q_bounds = Bounds::new(query, &q);
        let mut heap = BinaryHeap::<Reverse<Hit>>::new();
        let (mut scored, mut pruned) = (0, 0);
        for (id, (code, bound)) in self.index.codes.iter().zip(&self.bounds).enumerate() {
            if id.is_multiple_of(128) {
                check_cancel(cancel)?;
            }
            if heap.len() == k
                && heap.peek().is_some_and(|worst| {
                    bound.upper_score(code, &q, &q_bounds) < f64::from(worst.0.score)
                })
            {
                pruned += 1;
                continue;
            }
            let score = self.index.original[id]
                .iter()
                .zip(query)
                .map(|(a, b)| a * b)
                .sum();
            let hit = Hit { id, score };
            scored += 1;
            if heap.len() < k {
                heap.push(Reverse(hit));
            } else if heap.peek().is_some_and(|worst| hit > worst.0) {
                heap.pop();
                heap.push(Reverse(hit));
            }
        }
        check_cancel(cancel)?;
        let mut hits: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
        hits.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ExactSearch {
            hits,
            scored,
            pruned,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn truth(data: &[[f32; DIM]], query: &[f32; DIM], k: usize) -> Vec<Hit> {
        let mut all: Vec<_> = data
            .iter()
            .enumerate()
            .map(|(id, v)| Hit {
                id,
                score: v.iter().zip(query).map(|(a, b)| a * b).sum(),
            })
            .collect();
        all.sort_unstable_by(|a, b| b.cmp(a));
        all.truncate(k);
        all
    }

    #[test]
    fn exact_fallback_recovers_near_ties_and_preserves_id_ties() {
        let mut original = [[0.0; DIM]; 4];
        for v in &mut original {
            v[0] = 1.0;
        }
        original[0][1] = 0.1;
        original[1][1] = 0.1001;
        original[2][1] = 0.1001;
        original[3][1] = -0.2;
        let mut query = [0.0; DIM];
        query[1] = 1.0;
        let cancel = AtomicBool::new(false);
        let index = QuantizedIndex::build(&original, &cancel).unwrap();
        let pruner = ExactPruner::build(&index, &cancel).unwrap();
        for k in [1, 2, 3, 4, 10] {
            let result = pruner.search(&query, k, &cancel).unwrap();
            assert_eq!(result.hits, truth(&original, &query, k));
            assert_eq!(result.scored + result.pruned, original.len());
        }
        assert_eq!(pruner.search(&query, 1, &cancel).unwrap().hits[0].id, 1);
    }

    #[test]
    fn bounds_and_results_match_full_scan_for_mixed_scales_and_cancellation() {
        let mut state = 0x137c_79a5_88c3_b4d1u64;
        let mut random = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 40) as f32 / 8_388_608.0) - 1.0
        };
        let mut data: Vec<[f32; DIM]> = (0..1024)
            .map(|i| {
                let scale = [1.0, 0.01, 1e-20, 1e-40][i % 4];
                std::array::from_fn(|_| random() * scale)
            })
            .collect();
        data.push([0.0; DIM]);
        data.push([f32::from_bits(1); DIM]);
        let cancel = AtomicBool::new(false);
        let index = QuantizedIndex::build(&data, &cancel).unwrap();
        let pruner = ExactPruner::build(&index, &cancel).unwrap();
        let mut pruned = 0;
        for i in 0..16 {
            let scale = [1.0, 0.01, 1e-20, 1e-40][i % 4];
            let query = std::array::from_fn(|_| random() * scale);
            let q = encode(&query).unwrap();
            let qb = Bounds::new(&query, &q);
            for (id, v) in data.iter().enumerate() {
                let score: f32 = v.iter().zip(&query).map(|(a, b)| a * b).sum();
                assert!(
                    pruner.bounds[id].upper_score(&index.codes[id], &q, &qb) >= f64::from(score)
                );
            }
            for k in [1, 10, 100] {
                let result = pruner.search(&query, k, &cancel).unwrap();
                assert_eq!(result.hits, truth(&data, &query, k));
                assert_eq!(result.scored + result.pruned, data.len());
                pruned += result.pruned;
            }
        }
        assert!(pruned > 0);
        assert!(pruner.search(&[f32::NAN; DIM], 1, &cancel).is_err());
        assert!(pruner.search(&[0.0; DIM], 0, &cancel).is_err());
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(pruner.search(&[0.0; DIM], 1, &cancel).is_err());
        assert!(ExactPruner::build(&index, &cancel).is_err());
    }
}
