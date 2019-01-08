//! Product quantization.

use std::iter;
use std::iter::Sum;

use log::info;
use ndarray::{s, Array1, Array2, ArrayBase, ArrayView2, Axis, Data, Ix1, Ix2, NdFloat};
use ndarray_linalg::eigh::Eigh;
use ndarray_linalg::lapack_traits::UPLO;
use ndarray_linalg::types::Scalar;
use ndarray_parallel::prelude::*;
use num_traits::AsPrimitive;
use ordered_float::OrderedFloat;
use rand::{FromEntropy, Rng};
use rand_xorshift::XorShiftRng;
use rayon::prelude::*;

use crate::kmeans::{
    cluster_assignment, cluster_assignments, InitialCentroids, KMeansWithCentroids,
    NIterationsCondition, RandomInstanceCentroids,
};
use crate::linalg::Covariance;

/// Vector quantization.
pub trait QuantizeVector<A> {
    /// Quantize a batch of vectors.
    fn quantize_batch<S>(&self, x: ArrayBase<S, Ix2>) -> Array2<usize>
    where
        S: Data<Elem = A>;

    /// Quantize a vector.
    fn quantize_vector<S>(&self, x: ArrayBase<S, Ix1>) -> Array1<usize>
    where
        S: Data<Elem = A>;
}

/// Vector reconstruction.
pub trait ReconstructVector<A> {
    /// Reconstruct a vector.
    ///
    /// The vectors are reconstructed from the quantization indices.
    fn reconstruct_batch<S>(&self, quantized: ArrayBase<S, Ix2>) -> Array2<A>
    where
        S: Data<Elem = usize>;

    /// Reconstruct a batch of vectors.
    ///
    /// The vector is reconstructed from the quantization indices.
    fn reconstruct_vector<S>(&self, quantized: ArrayBase<S, Ix1>) -> Array1<A>
    where
        S: Data<Elem = usize>;
}

/// Optimized product quantizer for Gaussian variables (Ge et al., 2013).
///
/// A product quantizer is a vector quantizer that slices a vector and
/// assigns to the *i*-th slice the index of the nearest centroid of the
/// *i*-th subquantizer. Vector reconstruction consists of concatenating
/// the centroids that represent the slices.
///
/// This quantizer learns a orthonormal matrix that rotates the input
/// space in order to balance variances over subquantizers. The
/// optimization procedure assumes that the variables have a Gaussia
/// distribution.
pub struct GaussianOPQ<A> {
    projection: Array2<A>,
    pq: PQ<A>,
}

impl<A> GaussianOPQ<A>
where
    A: NdFloat + Scalar + Sum,
    A::Real: NdFloat,
    usize: AsPrimitive<A>,
{
    /// Train a product quantizer with the xorshift PRNG.
    ///
    /// Train a product quantizer with `n_subquantizers` subquantizers
    /// on `instances`. Each subquantizer has 2^`quantizer_bits`
    /// centroids.  The subquantizers are trained with `n_iterations`
    /// k-means iterations. Each subquantizer is trained `n_attempts`
    /// times, where the best clustering is used.
    pub fn train<S>(
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        instances: ArrayBase<S, Ix2>,
    ) -> Self
    where
        S: Sync + Data<Elem = A>,
    {
        let mut rng = XorShiftRng::from_entropy();
        Self::train_using(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            instances,
            &mut rng,
        )
    }

    /// Train a product quantizer.
    ///
    /// Train a product quantizer with `n_subquantizers` subquantizers
    /// on `instances`. Each subquantizer has 2^`quantizer_bits`
    /// centroids.  The subquantizers are trained with `n_iterations`
    /// k-means iterations. Each subquantizer is trained `n_attempts`
    /// times, where the best clustering is used.
    ///
    /// `rng` is used for picking the initial cluster centroids of
    /// each subquantizer.
    pub fn train_using<S>(
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        instances: ArrayBase<S, Ix2>,
        rng: &mut impl Rng,
    ) -> Self
    where
        S: Sync + Data<Elem = A>,
    {
        PQ::check_quantizer_invariants(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            instances.view(),
        );

        let projection = create_projection_matrix(instances.view(), n_subquantizers);
        let rx = instances.dot(&projection);
        let pq = PQ::train_using(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            rx,
            rng,
        );

        GaussianOPQ { pq, projection }
    }
}

impl<A> QuantizeVector<A> for GaussianOPQ<A>
where
    A: NdFloat + Sum,
{
    fn quantize_batch<S>(&self, x: ArrayBase<S, Ix2>) -> Array2<usize>
    where
        S: Data<Elem = A>,
    {
        let rx = x.dot(&self.projection);
        self.pq.quantize_batch(rx)
    }

    fn quantize_vector<S>(&self, x: ArrayBase<S, Ix1>) -> Array1<usize>
    where
        S: Data<Elem = A>,
    {
        let rx = x.dot(&self.projection);
        self.pq.quantize_vector(rx)
    }
}

impl<A> ReconstructVector<A> for GaussianOPQ<A>
where
    A: NdFloat + Sum,
{
    fn reconstruct_batch<S>(&self, quantized: ArrayBase<S, Ix2>) -> Array2<A>
    where
        S: Data<Elem = usize>,
    {
        self.pq
            .reconstruct_batch(quantized)
            .dot(&self.projection.t())
    }

    fn reconstruct_vector<S>(&self, quantized: ArrayBase<S, Ix1>) -> Array1<A>
    where
        S: Data<Elem = usize>,
    {
        self.pq
            .reconstruct_vector(quantized)
            .dot(&self.projection.t())
    }
}

/// Product quantizer (Jégou et al., 2011).
///
/// A product quantizer is a vector quantizer that slices a vector and
/// assigns to the *i*-th slice the index of the nearest centroid of the
/// *i*-th subquantizer. Vector reconstruction consists of concatenating
/// the centroids that represent the slices.
pub struct PQ<A> {
    quantizer_len: usize,
    quantizers: Vec<Array2<A>>,
}

impl<A> PQ<A>
where
    A: NdFloat + Sum,
    usize: AsPrimitive<A>,
{
    /// Train a product quantizer with the xorshift PRNG.
    ///
    /// Train a product quantizer with `n_subquantizers` subquantizers
    /// on `instances`. Each subquantizer has 2^`quantizer_bits`
    /// centroids.  The subquantizers are trained with `n_iterations`
    /// k-means iterations. Each subquantizer is trained `n_attempts`
    /// times, where the best clustering is used.
    pub fn train<S>(
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        instances: ArrayBase<S, Ix2>,
    ) -> Self
    where
        S: Sync + Data<Elem = A>,
    {
        let mut rng = XorShiftRng::from_entropy();
        Self::train_using(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            instances,
            &mut rng,
        )
    }

    /// Train a product quantizer.
    ///
    /// Train a product quantizer with `n_subquantizers` subquantizers
    /// on `instances`. Each subquantizer has 2^`quantizer_bits`
    /// centroids.  The subquantizers are trained with `n_iterations`
    /// k-means iterations. Each subquantizer is trained `n_attempts`
    /// times, where the best clustering is used.
    ///
    /// `rng` is used for picking the initial cluster centroids of
    /// each subquantizer.
    pub fn train_using<S>(
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        instances: ArrayBase<S, Ix2>,
        rng: &mut impl Rng,
    ) -> Self
    where
        S: Sync + Data<Elem = A>,
    {
        Self::check_quantizer_invariants(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            instances.view(),
        );

        let quantizers = initial_centroids(
            n_subquantizers,
            2usize.pow(n_subquantizer_bits),
            instances.view(),
            rng,
        );

        let quantizers = quantizers
            .into_par_iter()
            .enumerate()
            .map(|(idx, quantizer)| {
                Self::train_subquantizer(idx, quantizer, n_iterations, n_attempts, instances.view())
            })
            .collect();

        PQ {
            quantizer_len: instances.cols(),
            quantizers,
        }
    }

    fn check_quantizer_invariants(
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        instances: ArrayView2<A>,
    ) {
        assert!(
            n_subquantizers > 0 && n_subquantizers <= instances.cols(),
            "The number of subquantizers should at least be 1 and at most be {}.",
            instances.cols()
        );
        assert!(
            n_subquantizer_bits > 0,
            "Number of quantizer bits should at least be one."
        );
        assert!(
            instances.cols() % n_subquantizers == 0,
            "The number of subquantizers should evenly divide each instance."
        );
        assert!(
            n_iterations > 0,
            "The subquantizers should be optimized for at least one iteration."
        );
        assert!(
            n_attempts > 0,
            "The subquantizers should be optimized for at least one attempt."
        );
    }

    /// Train a subquantizer.
    ///
    /// `sq` is the index of the subquantizer, `sq_dims` the number of
    /// dimensions that are quantized, and `codebook_len` the code book
    /// size of the quantizer.
    fn train_subquantizer(
        idx: usize,
        quantizer: Array2<A>,
        n_iterations: usize,
        n_attempts: usize,
        instances: ArrayView2<A>,
    ) -> Array2<A> {
        assert!(n_attempts > 0, "Cannot train a subquantizer in 0 attempts.");

        info!("Training PQ subquantizer {}", idx);

        let sq_dims = quantizer.cols();

        let offset = idx * sq_dims;
        let sq_instances = instances.slice(s![.., offset..offset + sq_dims]);

        iter::repeat_with(|| {
            let mut quantizer = quantizer.to_owned();
            let loss = sq_instances.kmeans_with_centroids(
                Axis(0),
                quantizer.view_mut(),
                NIterationsCondition(n_iterations),
            );
            (loss, quantizer)
        })
        .take(n_attempts)
        .map(|(loss, quantizer)| (OrderedFloat(loss), quantizer))
        .min_by_key(|attempt| attempt.0)
        .unwrap()
        .1
    }

    /// Get the subquantizer centroids.
    pub fn subquantizers(&self) -> &[Array2<A>] {
        &self.quantizers
    }
}

impl<A> QuantizeVector<A> for PQ<A>
where
    A: NdFloat + Sum,
{
    fn quantize_batch<S>(&self, x: ArrayBase<S, Ix2>) -> Array2<usize>
    where
        S: Data<Elem = A>,
    {
        quantize_batch(&self.quantizers, self.quantizer_len, x)
    }

    fn quantize_vector<S>(&self, x: ArrayBase<S, Ix1>) -> Array1<usize>
    where
        S: Data<Elem = A>,
    {
        quantize(&self.quantizers, self.quantizer_len, x)
    }
}

impl<A> ReconstructVector<A> for PQ<A>
where
    A: NdFloat + Sum,
{
    fn reconstruct_batch<S>(&self, quantized: ArrayBase<S, Ix2>) -> Array2<A>
    where
        S: Data<Elem = usize>,
    {
        reconstruct_batch(&self.quantizers, quantized)
    }

    fn reconstruct_vector<S>(&self, quantized: ArrayBase<S, Ix1>) -> Array1<A>
    where
        S: Data<Elem = usize>,
    {
        reconstruct(&self.quantizers, quantized)
    }
}

fn bucket_eigenvalues<S, A>(eigenvalues: ArrayBase<S, Ix1>, n_buckets: usize) -> Vec<Vec<usize>>
where
    S: Data<Elem = A>,
    A: NdFloat,
{
    assert!(
        n_buckets > 0,
        "Cannot distribute eigenvalues over zero buckets."
    );
    assert!(
        eigenvalues.len() >= n_buckets,
        "At least one eigenvalue is required per bucket"
    );
    assert_eq!(
        eigenvalues.len() % n_buckets,
        0,
        "The number of eigenvalues should be a multiple of the number of buckets."
    );

    let mut eigenvalue_indices: Vec<usize> = (0..eigenvalues.len()).collect();
    eigenvalue_indices
        .sort_unstable_by(|l, r| OrderedFloat(eigenvalues[*l]).cmp(&OrderedFloat(eigenvalues[*r])));

    // Only handle positive values, to switch to log-space. This is
    // ok for our purposes, since we only eigendecompose covariance
    // matrices.
    assert!(
        eigenvalues[eigenvalue_indices[0]] >= A::zero(),
        "Bucketing is only supported for positive eigenvalues."
    );

    // Do eigenvalue multiplication in log-space to avoid over/underflow.
    let mut eigenvalues = eigenvalues.map(|&v| (v + A::epsilon()).ln());

    // Make values positive, this is so that we can treat eigenvalues
    // (0,1] and [1,] in the same manner.
    let smallest = eigenvalues
        .iter()
        .cloned()
        .min_by_key(|&v| OrderedFloat(v))
        .unwrap();
    eigenvalues.map_mut(|v| *v -= smallest);

    let mut assignments = vec![vec![]; n_buckets];
    let mut products = vec![A::zero(); n_buckets];
    let max_assignments = eigenvalues.len_of(Axis(0)) / n_buckets;

    while let Some(eigenvalue_idx) = eigenvalue_indices.pop() {
        // Find non-full bucket with the smallest product.
        let (idx, _) = assignments
            .iter()
            .enumerate()
            .filter(|(_, a)| a.len() < max_assignments)
            .min_by_key(|(idx, _)| OrderedFloat(products[*idx]))
            .unwrap();

        assignments[idx].push(eigenvalue_idx);
        products[idx] += eigenvalues[eigenvalue_idx];
    }

    assignments
}

fn create_projection_matrix<A>(instances: ArrayView2<A>, n_subquantizers: usize) -> Array2<A>
where
    A: NdFloat + Scalar,
    A::Real: NdFloat,
    usize: AsPrimitive<A>,
{
    info!(
        "Creating projection matrix ({} instances, {} dimensions, {} subquantizers)",
        instances.rows(),
        instances.cols(),
        n_subquantizers
    );

    // Compute the covariance matrix.
    let cov = instances.covariance(Axis(0));

    // Find eigenvalues/vectors.
    let (eigen_values, eigen_vectors) = cov.eigh(UPLO::Upper).unwrap();

    // Order principal components by their eigenvalues
    let buckets = bucket_eigenvalues(eigen_values.view(), n_subquantizers);

    let mut transformations = Array2::zeros((eigen_values.len(), eigen_values.len()));
    for (idx, direction_idx) in buckets.into_iter().flatten().enumerate() {
        transformations
            .index_axis_mut(Axis(1), idx)
            .assign(&eigen_vectors.index_axis(Axis(1), direction_idx));
    }

    transformations
}

fn initial_centroids<S, A>(
    n_subquantizers: usize,
    codebook_len: usize,
    instances: ArrayBase<S, Ix2>,
    rng: &mut impl Rng,
) -> Vec<Array2<A>>
where
    S: Data<Elem = A>,
    A: NdFloat,
{
    let sq_dims = instances.cols() / n_subquantizers;

    let mut random_centroids = RandomInstanceCentroids::new(rng);

    (0..n_subquantizers)
        .into_iter()
        .map(|sq| {
            let offset = sq * sq_dims;
            let sq_instances = instances.slice(s![.., offset..offset + sq_dims]);
            random_centroids.initial_centroids(sq_instances, Axis(0), codebook_len)
        })
        .collect()
}

fn quantize<A, S>(
    quantizers: &[Array2<A>],
    quantizer_len: usize,
    x: ArrayBase<S, Ix1>,
) -> Array1<usize>
where
    A: NdFloat + Sum,
    S: Data<Elem = A>,
{
    assert_eq!(
        quantizer_len,
        x.len(),
        "Quantizer and vector length mismatch"
    );

    let mut indices = Array1::zeros(quantizers.len());

    let mut offset = 0;
    for (quantizer, index) in quantizers.iter().zip(indices.iter_mut()) {
        let sub_vec = x.slice(s![offset..offset + quantizer.cols()]);
        *index = cluster_assignment(quantizer.view(), sub_vec);

        offset += quantizer.cols();
    }

    indices
}

fn quantize_batch<A, S>(
    quantizers: &[Array2<A>],
    quantizer_len: usize,
    x: ArrayBase<S, Ix2>,
) -> Array2<usize>
where
    A: NdFloat + Sum,
    S: Data<Elem = A>,
{
    assert_eq!(
        quantizer_len,
        x.cols(),
        "Quantizer and vector length mismatch"
    );

    let mut quantized = Array2::zeros((x.rows(), quantizers.len()));

    let mut offset = 0;
    for (quantizer, mut quantized) in quantizers.iter().zip(quantized.axis_iter_mut(Axis(1))) {
        let sub_matrix = x.slice(s![.., offset..offset + quantizer.cols()]);
        let assignments = cluster_assignments(quantizer.view(), sub_matrix, Axis(0));
        quantized.assign(&assignments);

        offset += quantizer.cols();
    }

    quantized
}

fn reconstruct<A, S>(quantizers: &[Array2<A>], quantized: ArrayBase<S, Ix1>) -> Array1<A>
where
    A: NdFloat,
    S: Data<Elem = usize>,
{
    assert_eq!(
        quantizers.len(),
        quantized.len(),
        "Quantization length does not match number of subquantizers"
    );

    let mut reconstruction = Array1::zeros((quantizers.len() * quantizers[0].cols(),));

    let mut offset = 0;
    for (&centroid, quantizer) in quantized.into_iter().zip(quantizers.iter()) {
        let mut sub_vec = reconstruction.slice_mut(s![offset..offset + quantizer.cols()]);
        sub_vec.assign(&quantizer.index_axis(Axis(0), centroid));
        offset += quantizer.cols();
    }

    reconstruction
}

fn reconstruct_batch<A, S>(quantizers: &[Array2<A>], quantized: ArrayBase<S, Ix2>) -> Array2<A>
where
    A: NdFloat,
    S: Data<Elem = usize>,
{
    assert_eq!(
        quantizers.len(),
        quantized.cols(),
        "Quantization length does not match number of subquantizers"
    );

    let mut reconstructions =
        Array2::zeros((quantized.rows(), quantizers.len() * quantizers[0].cols()));

    for (quantized, mut reconstruction) in
        quantized.outer_iter().zip(reconstructions.outer_iter_mut())
    {
        reconstruction.assign(&reconstruct(quantizers, quantized));
    }

    reconstructions
}

#[cfg(test)]
mod tests {
    use ndarray::{array, Array2};

    use super::{QuantizeVector, ReconstructVector, PQ};

    fn test_vectors() -> Array2<f32> {
        array![
            [0., 2., 0., -0.5, 0., 0.],
            [1., -0.2, 0., 0.5, 0.5, 0.],
            [-0.2, 0.2, 0., 0., -2., 0.],
            [1., 0.2, 0., 0., -2., 0.],
        ]
    }

    fn test_quantizations() -> Array2<usize> {
        array![[1, 1], [0, 1], [1, 0], [0, 0]]
    }

    fn test_reconstructions() -> Array2<f32> {
        array![
            [0., 1., 0., 0., 1., 0.],
            [1., 0., 0., 0., 1., 0.],
            [0., 1., 0., 1., -1., 0.],
            [1., 0., 0., 1., -1., 0.]
        ]
    }

    fn test_pq() -> PQ<f32> {
        let quantizers = vec![
            array![[1., 0., 0.], [0., 1., 0.]],
            array![[1., -1., 0.], [0., 1., 0.]],
        ];

        PQ {
            quantizer_len: 6,
            quantizers,
        }
    }

    #[test]
    fn bucket_eigenvalues() {
        // Some fake eigenvalues.
        let eigenvalues = array![0.2, 0.6, 0.4, 0.1, 0.3, 0.5];
        assert_eq!(
            super::bucket_eigenvalues(eigenvalues.view(), 3),
            vec![vec![1, 3], vec![5, 0], vec![2, 4]]
        );
    }

    #[test]
    fn bucket_large_eigenvalues() {
        let eigenvalues = array![11174., 23450., 30835., 1557., 32425., 5154.];
        assert_eq!(
            super::bucket_eigenvalues(eigenvalues.view(), 3),
            vec![vec![4, 3], vec![2, 5], vec![1, 0]]
        );
    }

    #[test]
    #[should_panic]
    fn bucket_eigenvalues_uneven() {
        // Some fake eigenvalues.
        let eigenvalues = array![0.2, 0.6, 0.4, 0.1, 0.3, 0.5];
        super::bucket_eigenvalues(eigenvalues.view(), 4);
    }

    #[test]
    fn quantize_batch_with_predefined_codebook() {
        let pq = test_pq();

        assert_eq!(pq.quantize_batch(test_vectors()), test_quantizations());
    }

    #[test]
    fn quantize_with_predefined_codebook() {
        let pq = test_pq();

        for (vector, quantization) in test_vectors()
            .outer_iter()
            .zip(test_quantizations().outer_iter())
        {
            assert_eq!(pq.quantize_vector(vector), quantization);
        }
    }

    #[test]
    fn reconstruct_batch_with_predefined_codebook() {
        let pq = test_pq();
        assert_eq!(
            pq.reconstruct_batch(test_quantizations()),
            test_reconstructions()
        );
    }

    #[test]
    fn reconstruct_with_predefined_codebook() {
        let pq = test_pq();

        for (quantization, reconstruction) in test_quantizations()
            .outer_iter()
            .zip(test_reconstructions().outer_iter())
        {
            assert_eq!(pq.reconstruct_vector(quantization), reconstruction);
        }
    }
}
