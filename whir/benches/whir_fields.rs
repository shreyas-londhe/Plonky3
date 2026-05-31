//! Cross-field benchmarks for the WHIR polynomial commitment scheme.
//!
//! Measures `commit` / `open` / `verify` separately, sweeping the polynomial
//! size `log_2(n)` from 14..=20 by default (up to 24 via env) across every field
//! backend that can instantiate this WHIR (Radix-2) path: BabyBear, KoalaBear, Goldilocks.
//!
//! Two requested fields are intentionally absent (skips logged at runtime):
//!   - Mersenne31: two-adicity 1 (a circle field); it does not implement
//!     `TwoAdicField` over its prime subgroup, so `WhirProver` cannot be
//!     instantiated against it at all.
//!   - BN254: `Bn254` does not implement `UniformSamplingField`, so no available
//!     challenger satisfies WHIR's `CanSampleUniformBits<F>` bound for a 254-bit
//!     challenge field. (Uniform sampling would require rejection sampling that
//!     is not implemented; adding it is out of scope.)
//!
//! Security is swept at two levels (100 and 128 bits). To reach 128-bit
//! *grind-feasible* security, WHIR needs an extension field large enough that the
//! folding/sumcheck proof-of-work (bounded by `|EF|`) is ~0 — otherwise the
//! required grind exceeds the base-field order and the prover either panics
//! (31-bit fields) or hangs grinding an infeasible PoW. Hence the large
//! extensions below; they are held CONSTANT across the two security levels so
//! security is the only axis that moves.
//!
//! The *unavoidable* per-field variation is the extension-field degree (deg-8 for
//! BabyBear/KoalaBear ≈ 248 bits, deg-5 for Goldilocks ≈ 320 bits — the smallest
//! TwoAdic binomial extensions reaching ≥128-bit) and the Merkle hash
//! width/digest. Both are dictated by the field and flagged in the report.
//!
//! Knobs (env vars):
//!   WHIR_BENCH_MIN_LOG / WHIR_BENCH_MAX_LOG  — size sweep bounds (default 14 / 20)
//!   WHIR_BENCH_POW                           — PoW bits (default 20; 0 disables grinding)
//!   WHIR_BENCH_PROOF_SIZES=<path>            — also emit a proof-size CSV to <path>
//!                                              (off by default; one proof built per cell)

use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
    BatchSize, BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main,
};
use p3_baby_bear::{BabyBear, Poseidon2BabyBear};
use p3_challenger::{
    CanObserve, CanSampleUniformBits, DuplexChallenger, FieldChallenger, GrindingChallenger,
};
use p3_commit::{Mmcs as P3Mmcs, MultilinearPcs};
use p3_dft::{Radix2DFTSmallBatch, TwoAdicSubgroupDft};
use p3_field::extension::BinomialExtensionField;
use p3_field::{ExtensionField, Field, TwoAdicField};
use p3_goldilocks::{Goldilocks, Poseidon2Goldilocks};
use p3_koala_bear::{KoalaBear, Poseidon2KoalaBear};
use p3_merkle_tree::MerkleTreeMmcs;
use p3_multilinear_util::poly::Poly;
use p3_symmetric::{PaddingFreeSponge, TruncatedPermutation};
use p3_whir::fiat_shamir::domain_separator::DomainSeparator;
use p3_whir::parameters::{FoldingFactor, ProtocolParameters, SecurityAssumption, WhirConfig};
use p3_whir::pcs::prover::WhirProver;
use p3_whir::sumcheck::layout::{Layout, SuffixProver, Table};
use p3_whir::sumcheck::{OpeningProtocol, TableShape, TableSpec};
use rand::SeedableRng;
use rand::rngs::SmallRng;
use serde::Serialize;

// WHIR parameters held CONSTANT across every field for a fair comparison.
const FOLDING: usize = 4;
const STARTING_LOG_INV_RATE: usize = 1;
const SOUNDNESS: SecurityAssumption = SecurityAssumption::CapacityBound;
// Both security levels are swept; the extension field is held constant per field
// (see below) so security is the only axis that moves.
const SECURITY_LEVELS: [usize; 2] = [100, 128];
// PoW ceiling. WHIR does not clamp per-round grind to this value, and its
// grinding witness lives in the *base* field. With the large extension fields
// used here (>=248 bits) the folding/sumcheck PoW is ~0, so only the query PoW
// remains: ~19 bits at security 128. A cap of 20 keeps `check_pow_bits()` true.
// (A 19-bit grind is ~0.5M hashes.)
//
// Overridable via WHIR_BENCH_POW. Set to 0 to disable grinding entirely (WHIR
// compensates with more queries): this removes the proof-of-work lottery so the
// `open` measurement reflects pure prover *arithmetic* scaling.
const DEFAULT_POW_BITS: usize = 20;
fn pow_bits() -> usize {
    env_usize("WHIR_BENCH_POW").unwrap_or(DEFAULT_POW_BITS)
}
// One opening claim of one column is enough to exercise the full pipeline.
const NUM_EVALUATIONS: usize = 1;

/// Concrete PCS type with the layout mode fixed to the SVO suffix prover.
type WhirPcs<EF, F, Dft, Mmcs, Ch> = WhirProver<EF, F, Dft, Mmcs, Ch, SuffixProver<F, EF>>;

/// Per-round inverse-rate schedule matching the default protocol parameters.
///
/// Round 0 starts at rate 1; each later round absorbs one fewer halving per
/// folded variable. Identical to the schedule in `whir_pcs.rs`.
fn default_round_log_inv_rates(num_variables: usize, folding_factor: &FoldingFactor) -> Vec<usize> {
    let (num_rounds, _) = folding_factor.compute_number_of_rounds(num_variables);
    let mut rates = Vec::with_capacity(num_rounds);
    let mut rate = 1;
    for round in 0..num_rounds {
        rate += folding_factor.at_round(round) - 1;
        rates.push(rate);
    }
    rates
}

/// Size sweep, optionally narrowed by env vars for iteration.
fn log_sizes() -> Vec<usize> {
    let lo = env_usize("WHIR_BENCH_MIN_LOG").unwrap_or(14);
    let hi = env_usize("WHIR_BENCH_MAX_LOG").unwrap_or(20);
    (lo..=hi).collect()
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|s| s.parse().ok())
}

/// Apply a size-dependent sample-size / measurement-time policy.
///
/// Big polynomials get the Criterion minimum (10 samples) to keep the suite
/// tractable; small ones keep a higher count for tighter estimates.
fn configure(group: &mut BenchmarkGroup<'_, WallTime>, log_size: usize) {
    let (samples, secs, warm) = match log_size {
        0..=18 => (20, 10, 2),
        19..=21 => (10, 20, 2),
        _ => (10, 30, 1),
    };
    group.sample_size(samples);
    group.measurement_time(Duration::from_secs(secs));
    group.warm_up_time(Duration::from_secs(warm));
}

/// Pre-built, field-generic benchmark fixture for a single polynomial size.
struct FieldBench<EF, F, Dft, Mmcs, Ch>
where
    F: TwoAdicField + Ord,
    EF: ExtensionField<F> + TwoAdicField,
    Dft: TwoAdicSubgroupDft<F>,
    Mmcs: P3Mmcs<F>,
    Ch: FieldChallenger<F>
        + GrindingChallenger<Witness = F>
        + CanSampleUniformBits<F>
        + CanObserve<Mmcs::Commitment>
        + Clone,
    SuffixProver<F, EF>: Layout<F, EF>,
{
    pcs: WhirPcs<EF, F, Dft, Mmcs, Ch>,
    witness: <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::Witness,
    protocol: OpeningProtocol,
    domain_separator: DomainSeparator<EF, F>,
    base_challenger: Ch,
}

impl<EF, F, Dft, Mmcs, Ch> FieldBench<EF, F, Dft, Mmcs, Ch>
where
    F: TwoAdicField + Ord,
    EF: ExtensionField<F> + TwoAdicField,
    Dft: TwoAdicSubgroupDft<F>,
    Mmcs: P3Mmcs<F>,
    Ch: FieldChallenger<F>
        + GrindingChallenger<Witness = F>
        + CanSampleUniformBits<F>
        + CanObserve<Mmcs::Commitment>
        + Clone,
    SuffixProver<F, EF>: Layout<F, EF>,
{
    /// Pristine challenger with the domain separator already absorbed.
    fn challenger(&self) -> Ch {
        let mut challenger = self.base_challenger.clone();
        self.domain_separator
            .observe_domain_separator(&mut challenger);
        challenger
    }

    /// Run one full commit→open→verify; panic on failure.
    ///
    /// Invoked once per (field, size) before any timing so a broken cell never
    /// produces a meaningless measurement.
    fn check(&self) {
        let mut challenger = self.challenger();
        let (commitment, prover_data) =
            <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::commit(
                &self.pcs,
                self.witness.clone(),
                &mut challenger,
            );
        let proof = <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::open(
            &self.pcs,
            prover_data,
            self.protocol.clone(),
            &mut challenger,
        );
        let mut verifier_challenger = self.challenger();
        <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::verify(
            &self.pcs,
            &commitment,
            &proof,
            &mut verifier_challenger,
            self.protocol.clone(),
        )
        .expect("correctness check failed before timing");
    }

    /// Time the commit phase.
    fn bench_commit(&self, group: &mut BenchmarkGroup<'_, WallTime>, label: &str) {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_batched(
                || (self.witness.clone(), self.challenger()),
                |(witness, mut challenger)| {
                    <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::commit(
                        &self.pcs,
                        witness,
                        &mut challenger,
                    )
                },
                BatchSize::PerIteration,
            );
        });
    }

    /// Time the open phase; commit runs in setup and is excluded.
    fn bench_prove(&self, group: &mut BenchmarkGroup<'_, WallTime>, label: &str) {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_batched(
                || {
                    let mut challenger = self.challenger();
                    let (_, prover_data) = <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<
                        EF,
                        Ch,
                    >>::commit(
                        &self.pcs, self.witness.clone(), &mut challenger
                    );
                    (prover_data, challenger)
                },
                |(prover_data, mut challenger)| {
                    <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::open(
                        &self.pcs,
                        prover_data,
                        self.protocol.clone(),
                        &mut challenger,
                    )
                },
                BatchSize::PerIteration,
            );
        });
    }

    /// Time the verify phase; the proof is built once outside the window.
    fn bench_verify(&self, group: &mut BenchmarkGroup<'_, WallTime>, label: &str) {
        let mut challenger = self.challenger();
        let (commitment, prover_data) =
            <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::commit(
                &self.pcs,
                self.witness.clone(),
                &mut challenger,
            );
        let proof = <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::open(
            &self.pcs,
            prover_data,
            self.protocol.clone(),
            &mut challenger,
        );

        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_batched(
                || self.challenger(),
                |mut challenger| {
                    <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::verify(
                        &self.pcs,
                        &commitment,
                        &proof,
                        &mut challenger,
                        self.protocol.clone(),
                    )
                    .unwrap();
                },
                BatchSize::PerIteration,
            );
        });
    }

    /// Serialized proof size in bytes (postcard), measured outside any timer.
    fn proof_bytes(&self) -> usize
    where
        <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::Proof: Serialize,
    {
        let mut challenger = self.challenger();
        let (_, prover_data) = <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::commit(
            &self.pcs,
            self.witness.clone(),
            &mut challenger,
        );
        let proof = <WhirPcs<EF, F, Dft, Mmcs, Ch> as MultilinearPcs<EF, Ch>>::open(
            &self.pcs,
            prover_data,
            self.protocol.clone(),
            &mut challenger,
        );
        postcard::to_allocvec(&proof)
            .expect("proof should serialize")
            .len()
    }
}

/// Generate a per-field module of concrete type aliases plus a `build` fn.
///
/// The timing logic lives once in the generic [`FieldBench`]; this only wires
/// the field-specific types (perm widths, extension degree, digest size) and
/// the permutation construction, which legitimately differ per field.
macro_rules! field_module {
    (
        $module:ident,
        field = $F:ty,
        ext = $EF:ty,
        hash = $Hash:ty,
        compress = $Compress:ty,
        challenger = $Chal:ty,
        mmcs = $Mmcs:ty,
        digest = $D:literal,
        build_perms = $build_perms:expr,
    ) => {
        mod $module {
            use super::*;

            type Fld = $F;
            type Ext = $EF;
            type Hash = $Hash;
            type Compress = $Compress;
            type Chal = $Chal;
            type Mmcs = $Mmcs;
            type Dft = Radix2DFTSmallBatch<Fld>;
            type Lay = SuffixProver<Fld, Ext>;
            type Pcs = WhirProver<Ext, Fld, Dft, Mmcs, Chal, Lay>;
            pub type Bench = FieldBench<Ext, Fld, Dft, Mmcs, Chal>;

            /// Build the fixture for a polynomial of `num_variables` variables
            /// at the given security level.
            pub fn build(num_variables: usize, security_level: usize) -> Bench {
                // Deterministic permutations; the seed fixes round constants only,
                // never the protocol cost.
                let mut perm_rng = SmallRng::seed_from_u64(1);
                let build_perms: fn(&mut SmallRng) -> (Mmcs, Chal) = $build_perms;
                let (mmcs, base_challenger) = build_perms(&mut perm_rng);

                let folding_factor = FoldingFactor::Constant(FOLDING);
                let params = ProtocolParameters {
                    security_level,
                    pow_bits: pow_bits(),
                    round_log_inv_rates: default_round_log_inv_rates(
                        num_variables,
                        &folding_factor,
                    ),
                    folding_factor,
                    soundness_type: SOUNDNESS,
                    starting_log_inv_rate: STARTING_LOG_INV_RATE,
                };

                let config = WhirConfig::<Ext, Fld, Chal>::new(num_variables, params);
                let dft = Dft::new(1 << config.max_fft_size());
                let pcs = Pcs::new(config, dft, mmcs);

                let mut data_rng = SmallRng::seed_from_u64(0xD157A1B);
                let table = Table::new(vec![Poly::<Fld>::rand(&mut data_rng, num_variables)]);
                let witness = <Lay as Layout<Fld, Ext>>::new_witness(vec![table], FOLDING);

                let protocol = OpeningProtocol::new(vec![TableSpec::new(
                    TableShape::new(num_variables, 1),
                    vec![vec![0]; NUM_EVALUATIONS],
                )]);

                let mut domain_separator = DomainSeparator::<Ext, Fld>::new(vec![]);
                pcs.add_domain_separator::<$D>(&mut domain_separator);

                FieldBench {
                    pcs,
                    witness,
                    protocol,
                    domain_separator,
                    base_challenger,
                }
            }
        }
    };
}

// --- Small Monty fields: degree-8 extension (~248 bits), Poseidon2 w16/w24, 8-elem digest. ---

field_module!(
    baby_bear,
    field = BabyBear,
    ext = BinomialExtensionField<BabyBear, 8>,
    hash = PaddingFreeSponge<Poseidon2BabyBear<24>, 24, 16, 8>,
    compress = TruncatedPermutation<Poseidon2BabyBear<16>, 2, 8, 16>,
    challenger = DuplexChallenger<BabyBear, Poseidon2BabyBear<16>, 16, 8>,
    mmcs = MerkleTreeMmcs<
        <BabyBear as Field>::Packing,
        <BabyBear as Field>::Packing,
        Hash,
        Compress,
        2,
        8,
    >,
    digest = 8,
    build_perms = |perm_rng| {
        let poseidon16 = Poseidon2BabyBear::<16>::new_from_rng_128(perm_rng);
        let poseidon24 = Poseidon2BabyBear::<24>::new_from_rng_128(perm_rng);
        let mmcs = Mmcs::new(Hash::new(poseidon24), Compress::new(poseidon16.clone()), 0);
        (mmcs, Chal::new(poseidon16))
    },
);

field_module!(
    koala_bear,
    field = KoalaBear,
    ext = BinomialExtensionField<KoalaBear, 8>,
    hash = PaddingFreeSponge<Poseidon2KoalaBear<24>, 24, 16, 8>,
    compress = TruncatedPermutation<Poseidon2KoalaBear<16>, 2, 8, 16>,
    challenger = DuplexChallenger<KoalaBear, Poseidon2KoalaBear<16>, 16, 8>,
    mmcs = MerkleTreeMmcs<
        <KoalaBear as Field>::Packing,
        <KoalaBear as Field>::Packing,
        Hash,
        Compress,
        2,
        8,
    >,
    digest = 8,
    build_perms = |perm_rng| {
        let poseidon16 = Poseidon2KoalaBear::<16>::new_from_rng_128(perm_rng);
        let poseidon24 = Poseidon2KoalaBear::<24>::new_from_rng_128(perm_rng);
        let mmcs = Mmcs::new(Hash::new(poseidon24), Compress::new(poseidon16.clone()), 0);
        (mmcs, Chal::new(poseidon16))
    },
);

// --- Goldilocks: degree-5 extension (~320 bits), single Poseidon2 w8 perm, 4-elem digest. ---

field_module!(
    goldilocks,
    field = Goldilocks,
    ext = BinomialExtensionField<Goldilocks, 5>,
    hash = PaddingFreeSponge<Poseidon2Goldilocks<8>, 8, 4, 4>,
    compress = TruncatedPermutation<Poseidon2Goldilocks<8>, 2, 4, 8>,
    challenger = DuplexChallenger<Goldilocks, Poseidon2Goldilocks<8>, 8, 4>,
    mmcs = MerkleTreeMmcs<
        <Goldilocks as Field>::Packing,
        <Goldilocks as Field>::Packing,
        Hash,
        Compress,
        2,
        4,
    >,
    digest = 4,
    build_perms = |perm_rng| {
        let perm = Poseidon2Goldilocks::<8>::new_from_rng_128(perm_rng);
        let mmcs = Mmcs::new(Hash::new(perm.clone()), Compress::new(perm.clone()), 0);
        (mmcs, Chal::new(perm))
    },
);

// BN254 is intentionally absent: see the module docstring and `log_skips`.
// WHIR's query sampler requires `F: PrimeField64`, which the 254-bit `Bn254`
// cannot implement, so `WhirProver`'s `CanSampleUniformBits<F>` bound is
// unsatisfiable for it.

/// Sweep one field's `commit` benchmarks across all (security, size) cells,
/// gating each on a correctness check.
macro_rules! bench_commit_field {
    ($group:expr, $module:ident, $name:literal) => {
        for &sec in &SECURITY_LEVELS {
            for n in log_sizes() {
                configure($group, n);
                let bench = $module::build(n, sec);
                bench.check();
                bench.bench_commit($group, &format!(concat!($name, "/s{}/{}"), sec, n));
            }
        }
    };
}

/// Sweep one field's `open` (or `verify`) benchmarks across all (security, size) cells.
macro_rules! bench_field {
    ($group:expr, $module:ident, $name:literal, $method:ident) => {
        for &sec in &SECURITY_LEVELS {
            for n in log_sizes() {
                configure($group, n);
                let bench = $module::build(n, sec);
                bench.$method($group, &format!(concat!($name, "/s{}/{}"), sec, n));
            }
        }
    };
}

/// Log the incompatible-field skips once per run, with the reason for each.
fn log_skips() {
    for n in log_sizes() {
        eprintln!(
            "SKIP m31 log={n}: Mersenne31 has two-adicity 1 (circle field); \
             it does not implement TwoAdicField over its prime subgroup, so the \
             Radix-2 WHIR path cannot be instantiated against it."
        );
        eprintln!(
            "SKIP bn254 log={n}: WHIR's query sampler requires F: PrimeField64 \
             (CanSampleUniformBits is built on a [u64; 64] rejection table). Bn254 \
             is 254-bit and cannot implement PrimeField64, so WHIR over BN254 is \
             architecturally impossible here without a new >64-bit uniform sampler."
        );
    }
}

fn commit_benches(c: &mut Criterion) {
    log_skips();
    let mut group = c.benchmark_group("whir_fields/commit");
    bench_commit_field!(&mut group, baby_bear, "baby_bear");
    bench_commit_field!(&mut group, koala_bear, "koala_bear");
    bench_commit_field!(&mut group, goldilocks, "goldilocks");
    group.finish();
}

fn open_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("whir_fields/open");
    bench_field!(&mut group, baby_bear, "baby_bear", bench_prove);
    bench_field!(&mut group, koala_bear, "koala_bear", bench_prove);
    bench_field!(&mut group, goldilocks, "goldilocks", bench_prove);
    group.finish();
}

fn verify_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("whir_fields/verify");
    bench_field!(&mut group, baby_bear, "baby_bear", bench_verify);
    bench_field!(&mut group, koala_bear, "koala_bear", bench_verify);
    bench_field!(&mut group, goldilocks, "goldilocks", bench_verify);
    group.finish();
}

/// Optional, env-gated pass that writes a `field,log_size,proof_bytes` CSV.
///
/// Off by default — set `WHIR_BENCH_PROOF_SIZES=<path>` to enable. It builds one
/// proof per cell, so it is gated to avoid paying that cost on every `cargo bench`.
fn proof_size_report(_c: &mut Criterion) {
    let Some(path) = std::env::var("WHIR_BENCH_PROOF_SIZES").ok() else {
        return;
    };

    let mut csv = String::from("field,security,log_size,proof_bytes\n");
    macro_rules! sizes_for {
        ($module:ident, $name:literal) => {
            for &sec in &SECURITY_LEVELS {
                for n in log_sizes() {
                    let bytes = $module::build(n, sec).proof_bytes();
                    csv.push_str(&format!(concat!($name, ",{},{},{}\n"), sec, n, bytes));
                }
            }
        };
    }
    sizes_for!(baby_bear, "baby_bear");
    sizes_for!(koala_bear, "koala_bear");
    sizes_for!(goldilocks, "goldilocks");

    std::fs::write(&path, csv).expect("proof-size CSV should write");
    eprintln!("wrote proof sizes to {path}");
}

criterion_group!(
    benches,
    proof_size_report,
    commit_benches,
    open_benches,
    verify_benches
);
criterion_main!(benches);
