//! Experimental scalar-quantized vector shortlist, not used by production search.
//! Linear scan of compact codes followed by exact reranking. Not HNSW.
use anyhow::{ensure, Result};
use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

pub const DIM: usize = 512;
pub const MAX_VECTORS: usize = 250_000;
pub const MAX_SHORTLIST: usize = 20_000;

struct Code {
    values: [i8; DIM],
    scale: f32,
}

fn encode(vector: &[f32; DIM]) -> Result<Code> {
    ensure!(
        vector.iter().all(|v| v.is_finite() && v.abs() <= 1.01),
        "invalid vector components"
    );
    let maximum = vector.iter().map(|v| v.abs()).fold(0.0_f32, f32::max);
    let values = if maximum == 0.0 {
        [0; DIM]
    } else {
        vector.map(|v| ((v / maximum) * 127.0).round() as i8)
    };
    Ok(Code {
        values,
        scale: maximum / 127.0,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub id: usize,
    pub score: f32,
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Hit {}
impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.id.cmp(&self.id))
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(AtomicOrdering::Acquire),
        "vector retrieval cancelled"
    );
    Ok(())
}

/// Dense IDs identify original vectors. Borrowing the immutable original slice
/// prevents accidental reranking against a different snapshot/generation.
pub struct QuantizedIndex<'a> {
    original: &'a [[f32; DIM]],
    codes: Vec<Code>,
}

impl<'a> QuantizedIndex<'a> {
    pub fn build(original: &'a [[f32; DIM]], cancel: &AtomicBool) -> Result<Self> {
        ensure!(
            original.len() <= MAX_VECTORS,
            "vector index capacity exceeded"
        );
        check_cancel(cancel)?;
        let mut codes = Vec::new();
        codes.try_reserve_exact(original.len())?;
        for (id, vector) in original.iter().enumerate() {
            if id.is_multiple_of(128) {
                check_cancel(cancel)?;
            }
            codes.push(encode(vector)?);
        }
        check_cancel(cancel)?;
        Ok(Self { original, codes })
    }

    pub fn allocated_bytes(&self) -> usize {
        self.codes.capacity() * std::mem::size_of::<Code>()
    }

    /// Approximate vector IDs only; no exact global result count.
    pub fn shortlist(
        &self,
        query: &[f32; DIM],
        budget: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<usize>> {
        ensure!(
            (1..=MAX_SHORTLIST).contains(&budget),
            "invalid shortlist budget"
        );
        check_cancel(cancel)?;
        let q = encode(query)?;
        let mut heap = BinaryHeap::<std::cmp::Reverse<Hit>>::new();
        for (id, code) in self.codes.iter().enumerate() {
            if id.is_multiple_of(128) {
                check_cancel(cancel)?;
            }
            let sum: i32 = code
                .values
                .iter()
                .zip(q.values.iter())
                .map(|(&a, &b)| i32::from(a) * i32::from(b))
                .sum();
            let score = (sum as f32 * code.scale) * q.scale;
            let hit = Hit { id, score };
            if heap.len() < budget {
                heap.push(std::cmp::Reverse(hit));
            } else if heap.peek().is_some_and(|last| hit > last.0) {
                heap.pop();
                heap.push(std::cmp::Reverse(hit));
            }
        }
        check_cancel(cancel)?;
        let mut ranked: Vec<_> = heap.into_iter().map(|hit| hit.0).collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ranked.into_iter().map(|hit| hit.id).collect())
    }

    /// Exact original f32 scores within the approximate shortlist only.
    pub fn search(
        &self,
        query: &[f32; DIM],
        k: usize,
        budget: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<Hit>> {
        ensure!(k > 0 && k <= budget, "invalid result count");
        let candidates = self.shortlist(query, budget, cancel)?;
        let mut results = Vec::with_capacity(candidates.len());
        for id in candidates {
            check_cancel(cancel)?;
            let score = self.original[id]
                .iter()
                .zip(query)
                .map(|(a, b)| a * b)
                .sum();
            results.push(Hit { id, score });
        }
        results.sort_unstable_by(|a, b| b.cmp(a));
        results.truncate(k);
        check_cancel(cancel)?;
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortlist_is_bounded_and_reranks_with_original_scores() {
        let mut original = vec![[0.0; DIM]; 100];
        for (i, v) in original.iter_mut().enumerate() {
            v[0] = i as f32 / 100.0;
            v[1] = -0.2;
        }
        let mut q = [0.0; DIM];
        q[0] = 1.0;
        let cancel = AtomicBool::new(false);
        let index = QuantizedIndex::build(&original, &cancel).unwrap();
        assert_eq!(index.shortlist(&q, 10, &cancel).unwrap().len(), 10);
        let hits = index.search(&q, 5, 10, &cancel).unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![99, 98, 97, 96, 95]
        );
        assert_eq!(hits[0].score, original[99][0]);
        assert!(index.shortlist(&q, 0, &cancel).is_err());
        assert!(index.shortlist(&q, MAX_SHORTLIST + 1, &cancel).is_err());
        cancel.store(true, AtomicOrdering::Release);
        assert!(index.search(&q, 5, 10, &cancel).is_err());
    }

    #[test]
    fn near_ties_can_miss_exact_neighbor_and_wider_budget_recovers_it() {
        let mut original = [[0.0; DIM]; 2];
        original[0][0] = 1.0;
        original[1][0] = 1.0;
        original[0][1] = 0.1000;
        original[1][1] = 0.1001;
        let mut query = [0.0; DIM];
        query[1] = 1.0;
        let cancel = AtomicBool::new(false);
        let index = QuantizedIndex::build(&original, &cancel).unwrap();
        assert_eq!(index.search(&query, 1, 1, &cancel).unwrap()[0].id, 0);
        assert_eq!(index.search(&query, 1, 2, &cancel).unwrap()[0].id, 1);
    }

    #[test]
    fn invalid_and_tied_vectors_have_explicit_behavior() {
        let cancel = AtomicBool::new(false);
        let mut bad = [0.0; DIM];
        bad[3] = f32::NAN;
        assert!(QuantizedIndex::build(&[bad], &cancel).is_err());
        bad[3] = 2.0;
        assert!(QuantizedIndex::build(&[bad], &cancel).is_err());
        let original = vec![[0.0; DIM]; 8];
        let index = QuantizedIndex::build(&original, &cancel).unwrap();
        assert_eq!(
            index.shortlist(&original[0], 3, &cancel).unwrap(),
            vec![0, 1, 2]
        );
        assert!(index.shortlist(&bad, 3, &cancel).is_err());
        let mut tiny = [0.0; DIM];
        tiny[0] = f32::from_bits(1);
        assert!(QuantizedIndex::build(&[tiny], &cancel).is_ok());
    }
}

mod exact;
pub use exact::{ExactPruner, ExactSearch};
