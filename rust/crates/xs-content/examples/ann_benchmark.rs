//! Deterministic vector-only A/B benchmark. No model inference or user documents.
use anyhow::{ensure, Result};
use std::{collections::BinaryHeap, sync::atomic::AtomicBool, time::Instant};
use xs_content::experimental_ann::{Hit, QuantizedIndex, DIM, MAX_VECTORS};

fn random(state: &mut u64) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    ((*state >> 40) as f32 / 8_388_608.0) - 1.0
}

fn normalized(mut v: [f32; DIM]) -> [f32; DIM] {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
        *x /= norm;
    }
    v
}

fn exact(data: &[[f32; DIM]], query: &[f32; DIM], k: usize) -> Vec<Hit> {
    let mut heap = BinaryHeap::<std::cmp::Reverse<Hit>>::new();
    for (id, vector) in data.iter().enumerate() {
        let hit = Hit {
            id,
            score: vector.iter().zip(query).map(|(a, b)| a * b).sum(),
        };
        if heap.len() < k {
            heap.push(std::cmp::Reverse(hit));
        } else if heap.peek().is_some_and(|last| hit > last.0) {
            heap.pop();
            heap.push(std::cmp::Reverse(hit));
        }
    }
    let mut result: Vec<_> = heap.into_iter().map(|h| h.0).collect();
    result.sort_unstable_by(|a, b| b.cmp(a));
    result
}

fn p95(times: &mut [f64]) -> f64 {
    times.sort_by(f64::total_cmp);
    times[(times.len() * 95).div_ceil(100) - 1]
}

fn run(count: usize, clustered: bool) -> Result<serde_json::Value> {
    let mut state = 0x137c_79a5_88c3_b4d1;
    let centers: Vec<_> = (0..64)
        .map(|_| std::array::from_fn::<_, DIM, _>(|_| random(&mut state)))
        .collect();
    let mut data = Vec::with_capacity(count);
    for i in 0..count {
        let center = &centers[i % centers.len()];
        data.push(normalized(std::array::from_fn(|d| {
            let noise = random(&mut state);
            if clustered {
                center[d] + noise * 0.35
            } else {
                noise
            }
        })));
    }
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let index = QuantizedIndex::build(&data, &cancel)?;
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut exact_times = Vec::new();
    let mut approx_times = [Vec::new(), Vec::new(), Vec::new()];
    let mut recalls = [Vec::new(), Vec::new(), Vec::new()];
    let budgets = [32, 128, 512];
    let k = 10;
    for q in 0..32 {
        // Queries are generated independently, not copied from the indexed set.
        let query = normalized(std::array::from_fn(|d| {
            let noise = random(&mut state);
            if clustered {
                centers[q % centers.len()][d] + noise * 0.35
            } else {
                noise
            }
        }));
        let started = Instant::now();
        let truth = exact(&data, &query, k);
        exact_times.push(started.elapsed().as_secs_f64() * 1000.0);
        for (i, budget) in budgets.into_iter().enumerate() {
            let started = Instant::now();
            let result = index.search(&query, k, budget, &cancel)?;
            approx_times[i].push(started.elapsed().as_secs_f64() * 1000.0);
            ensure!(result.len() == k, "short result");
            let matched = result
                .iter()
                .filter(|hit| truth.iter().any(|t| t.id == hit.id))
                .count();
            for hit in &result {
                let score: f32 = data[hit.id].iter().zip(query).map(|(a, b)| a * b).sum();
                ensure!(hit.score == score, "rerank score mismatch");
            }
            recalls[i].push(matched as f64 / k as f64);
        }
    }
    let variants: Vec<_> = budgets
        .into_iter()
        .enumerate()
        .map(|(i, budget)| {
            serde_json::json!({
                "shortlist":budget, "p95_ms":p95(&mut approx_times[i]),
                "mean_recall_at_10":recalls[i].iter().sum::<f64>() / recalls[i].len() as f64,
                "min_recall_at_10":recalls[i].iter().copied().fold(1.0_f64,f64::min),
                "per_query_recall":recalls[i],
            })
        })
        .collect();
    Ok(
        serde_json::json!({"dataset":if clustered {"synthetic_clustered"} else {"synthetic_uniform"},
        "vectors":count,"dimension":DIM,"queries":32,"build_ms":build_ms,
        "code_bytes":index.allocated_bytes(),"original_bytes":data.len()*DIM*4,
        "exact_p95_ms":p95(&mut exact_times),"variants":variants}),
    )
}

fn main() -> Result<()> {
    let count = std::env::args()
        .nth(1)
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(100_000usize);
    ensure!(
        (512..=MAX_VECTORS).contains(&count),
        "vector count out of range"
    );
    let result = [run(count, false)?, run(count, true)?];
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
