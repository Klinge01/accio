use half::f16;

const CANDIDATES: usize = 200;

pub struct VecIndex {
    ids: Vec<i64>,
    vectors: Vec<f16>,
    codes: Vec<u64>,
    dim: usize,
}

fn code(vector: impl Iterator<Item = bool>, words: &mut [u64]) {
    for (index, positive) in vector.enumerate() {
        words[index / 64] |= u64::from(positive) << (index % 64);
    }
}

impl VecIndex {
    pub fn empty(dim: usize) -> Self {
        Self::build(Vec::new(), Vec::new(), dim)
    }

    pub fn build(ids: Vec<i64>, vectors: Vec<f16>, dim: usize) -> Self {
        assert!(dim > 0 && vectors.len() == ids.len() * dim);
        let words = dim.div_ceil(64);
        let mut codes = vec![0; ids.len() * words];
        for (vector, bits) in vectors.chunks_exact(dim).zip(codes.chunks_exact_mut(words)) {
            code(vector.iter().map(|value| !value.is_sign_negative()), bits);
        }
        Self {
            ids,
            vectors,
            codes,
            dim,
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        if self.is_empty() || k == 0 {
            return Vec::new();
        }
        assert_eq!(query.len(), self.dim);
        let mut query_code = vec![0; self.dim.div_ceil(64)];
        code(
            query.iter().map(|value| !value.is_sign_negative()),
            &mut query_code,
        );
        let mut approximate: Vec<_> = self
            .codes
            .chunks_exact(query_code.len())
            .enumerate()
            .map(|(index, bits)| {
                let distance: u32 = bits
                    .iter()
                    .zip(&query_code)
                    .map(|(a, b)| (a ^ b).count_ones())
                    .sum();
                (distance, index)
            })
            .collect();
        if approximate.len() > CANDIDATES {
            approximate.select_nth_unstable(CANDIDATES);
            approximate.truncate(CANDIDATES);
        }
        let mut exact: Vec<_> = approximate
            .into_iter()
            .map(|(_, index)| {
                let offset = index * self.dim;
                let dot: f32 = self.vectors[offset..offset + self.dim]
                    .iter()
                    .zip(query)
                    .map(|(value, q)| value.to_f32() * q)
                    .sum();
                (self.ids[index], dot)
            })
            .collect();
        exact.sort_unstable_by(|(id_a, a), (id_b, b)| b.total_cmp(a).then(id_a.cmp(id_b)));
        exact.truncate(k);
        exact
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::normalize;
    use std::time::Instant;

    fn random(state: &mut u64) -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        ((*state >> 40) as f32 / (1 << 23) as f32) - 1.0
    }

    #[test]
    fn vindex_exact_match_first_and_empty() {
        let query = [0.5, -0.5, 0.5, -0.5, 0.0];
        let vectors = [-0.5, 0.5, 0.5, 0.5, 0.0]
            .into_iter()
            .chain(query)
            .map(f16::from_f32)
            .collect();
        let index = VecIndex::build(vec![1, 2], vectors, query.len());
        assert_eq!(index.search(&query, 1), [(2, 1.0)]);
        assert!(VecIndex::empty(query.len()).search(&query, 10).is_empty());
    }

    #[test]
    fn vindex_recall_vs_bruteforce_on_clustered_vectors() {
        const DIM: usize = 512;
        let mut seed = 728;
        let centers: Vec<Vec<f32>> = (0..20)
            .map(|_| (0..DIM).map(|_| random(&mut seed)).collect())
            .collect();
        let vectors: Vec<Vec<f32>> = centers
            .iter()
            .flat_map(|center| (0..100).map(move |_| center))
            .map(|center| {
                let mut vector: Vec<_> = center
                    .iter()
                    .map(|&value| value + 0.22 * random(&mut seed))
                    .collect();
                normalize(&mut vector);
                vector
            })
            .collect();
        let ids = (0..vectors.len() as i64).collect::<Vec<_>>();
        let index = VecIndex::build(
            ids,
            vectors
                .iter()
                .flatten()
                .copied()
                .map(f16::from_f32)
                .collect(),
            DIM,
        );
        let mut found = 0;
        for center in centers {
            let mut query: Vec<_> = center
                .iter()
                .map(|&x| x + 0.22 * random(&mut seed))
                .collect();
            normalize(&mut query);
            let mut exact: Vec<_> = vectors
                .iter()
                .enumerate()
                .map(|(id, vector)| {
                    let score = query.iter().zip(vector).map(|(a, b)| a * b).sum::<f32>();
                    (id as i64, score)
                })
                .collect();
            exact.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let retrieved = index.search(&query, 10);
            found += exact[..10]
                .iter()
                .filter(|(id, _)| retrieved.iter().any(|(other, _)| other == id))
                .count();
        }
        assert!(found >= 180, "recall@10: {}", found as f32 / 200.0);
    }

    #[test]
    #[ignore = "measures independent-vector recall without imposing an exact-search target"]
    fn vindex_random_recall() {
        const DIM: usize = 512;
        const COUNT: usize = 20_000;
        let mut seed = 728;
        let mut vector = || {
            let mut vector: Vec<_> = (0..DIM).map(|_| random(&mut seed)).collect();
            normalize(&mut vector);
            vector
        };
        let corpus: Vec<_> = (0..COUNT).map(|_| vector()).collect();
        let index = VecIndex::build(
            (0..COUNT as i64).collect(),
            corpus
                .iter()
                .flatten()
                .copied()
                .map(f16::from_f32)
                .collect(),
            DIM,
        );
        let mut found = 0;
        for _ in 0..50 {
            let query = vector();
            let mut exact: Vec<_> = corpus
                .iter()
                .enumerate()
                .map(|(id, doc)| {
                    let score: f32 = query.iter().zip(doc).map(|(a, b)| a * b).sum();
                    (id as i64, score)
                })
                .collect();
            exact.select_nth_unstable_by(10, |a, b| b.1.total_cmp(&a.1));
            let approximate = index.search(&query, 10);
            found += exact[..10]
                .iter()
                .filter(|(id, _)| approximate.iter().any(|(other, _)| other == id))
                .count();
        }
        println!("random-vector recall@10: {:.1}%", found as f32 / 5.0);
    }

    #[test]
    #[ignore = "release throughput benchmark"]
    fn vindex_bench() {
        let dim = 512;
        let count = 150_000;
        let mut seed = 728;
        let mut values = Vec::with_capacity(count * dim);
        let mut vector = vec![0.0; dim];
        for _ in 0..count {
            vector.fill_with(|| random(&mut seed));
            normalize(&mut vector);
            values.extend(vector.iter().copied().map(f16::from_f32));
        }
        let index = VecIndex::build((0..count as i64).collect(), values, dim);
        let start = Instant::now();
        for _ in 0..50 {
            std::hint::black_box(index.search(&vector, 50));
        }
        println!(
            "mean search {:.0} µs",
            start.elapsed().as_secs_f64() * 1e6 / 50.0
        );
    }
}
