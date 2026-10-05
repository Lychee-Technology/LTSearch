//! Quality and latency statistics. Rankings are lists of record indices,
//! best first.

/// The fraction of the exact top-`k` that `ranking`'s top-`k` contains.
/// Shorter lists are cut, not padded: the denominator is the number of exact
/// results available, at most `k`.
pub fn recall_at(k: usize, exact: &[usize], ranking: &[usize]) -> f64 {
    let truth = &exact[..k.min(exact.len())];
    if truth.is_empty() {
        return 1.0;
    }
    let found = ranking[..k.min(ranking.len())]
        .iter()
        .filter(|index| truth.contains(index))
        .count();
    found as f64 / truth.len() as f64
}

/// NDCG@`k` with the exact inner product as graded relevance.
///
/// `relevance` holds the exact inner products of `ranking`'s top-`k` in rank
/// order, `ideal` those of the exact top-`k`, best first. Gains are clamped at
/// zero, since DCG assumes non-negative gains; a query whose ideal gains are
/// all zero has nothing to find and scores 1.
pub fn ndcg_at(k: usize, relevance: &[f64], ideal: &[f64]) -> f64 {
    let ideal_dcg = dcg(k, ideal);
    if ideal_dcg == 0.0 {
        return 1.0;
    }
    dcg(k, relevance) / ideal_dcg
}

fn dcg(k: usize, relevance: &[f64]) -> f64 {
    relevance
        .iter()
        .take(k)
        .enumerate()
        .map(|(rank, gain)| gain.max(0.0) / ((rank + 2) as f64).log2())
        .sum()
}

/// Running error of score estimates against exact inner products.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoreError {
    pairs: u64,
    sum: f64,
    sum_squares: f64,
}

impl ScoreError {
    pub fn add(&mut self, estimate: f32, exact: f32) {
        let error = f64::from(estimate) - f64::from(exact);
        self.pairs += 1;
        self.sum += error;
        self.sum_squares += error * error;
    }

    pub fn merge(self, other: &Self) -> Self {
        Self {
            pairs: self.pairs + other.pairs,
            sum: self.sum + other.sum,
            sum_squares: self.sum_squares + other.sum_squares,
        }
    }

    pub fn pairs(&self) -> u64 {
        self.pairs
    }

    /// Mean of estimate − exact.
    pub fn bias(&self) -> f64 {
        self.sum / self.pairs as f64
    }

    pub fn rmse(&self) -> f64 {
        (self.sum_squares / self.pairs as f64).sqrt()
    }
}

/// The nearest-rank `p`-th percentile of `values`, which must not be empty.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    assert!(!values.is_empty(), "percentile of no values");
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

pub fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    #[test]
    fn recall_counts_exact_results_found_regardless_of_order() {
        let exact = [7, 3, 9, 1];
        close(recall_at(1, &exact, &[7, 0]), 1.0);
        close(recall_at(1, &exact, &[3, 7]), 0.0);
        close(recall_at(2, &exact, &[3, 7]), 1.0);
        // 9 and 1 are found, 0 and 2 are not.
        close(recall_at(4, &exact, &[0, 9, 2, 1]), 0.5);
        // A result past position k doesn't count.
        close(recall_at(2, &exact, &[0, 2, 7]), 0.0);
    }

    #[test]
    fn recall_cuts_short_lists() {
        close(recall_at(10, &[4, 5], &[5]), 0.5);
        close(recall_at(10, &[], &[1]), 1.0);
    }

    #[test]
    fn ndcg_matches_a_hand_computed_case() {
        // DCG of gains (0.5, 0.9) = 0.5/log2(2) + 0.9/log2(3)
        // ideal (0.9, 0.5)        = 0.9/log2(2) + 0.5/log2(3)
        let dcg = 0.5 + 0.9 / 3f64.log2();
        let ideal = 0.9 + 0.5 / 3f64.log2();
        close(ndcg_at(2, &[0.5, 0.9], &[0.9, 0.5]), dcg / ideal);
        close(ndcg_at(2, &[0.9, 0.5], &[0.9, 0.5]), 1.0);
    }

    #[test]
    fn ndcg_only_counts_the_top_k() {
        close(ndcg_at(1, &[0.9, 0.1], &[0.9, 0.8]), 1.0);
        close(ndcg_at(1, &[0.45, 0.9], &[0.9, 0.8]), 0.5);
    }

    #[test]
    fn ndcg_clamps_negative_gains() {
        // A negative inner product gains nothing, the same as a zero one.
        close(ndcg_at(2, &[-0.4, 0.6], &[0.6, -0.4]), 1.0 / 3f64.log2());
        close(ndcg_at(2, &[-0.1, -0.2], &[-0.1, -0.2]), 1.0);
    }

    #[test]
    fn score_error_is_bias_and_rmse_of_the_differences() {
        let mut error = ScoreError::default();
        // Differences +0.5, -0.25, +1.0.
        error.add(1.5, 1.0);
        error.add(0.25, 0.5);
        error.add(1.0, 0.0);
        assert_eq!(error.pairs(), 3);
        close(error.bias(), 1.25 / 3.0);
        close(error.rmse(), ((0.25 + 0.0625 + 1.0) / 3.0f64).sqrt());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let values: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        close(percentile(&values, 50.0), 50.0);
        close(percentile(&values, 95.0), 95.0);
        close(percentile(&values, 100.0), 100.0);
        close(percentile(&values, 0.0), 1.0);
        close(percentile(&[3.0, 1.0, 2.0], 50.0), 2.0);
    }
}
