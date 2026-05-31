# WHIR vs Barretenberg (KZG/UltraHonk) — same-machine comparison

Both measured on the **same Apple M3 Pro (12 cores)** to remove the hardware confound. bb was
re-run locally from [`suyash67/noir-rsa-passport-bench`](https://github.com/suyash67/noir-rsa-passport-bench)
(its published numbers were on an EPYC 7R13).

- **bb:** UltraHonk over BN254 (KZG / Shplonk / Gemini PCS), `bb 4.0.0-nightly.20260120`, noir
  `1.0.0-beta.19`. Circuit = RSA ePassport `complete_age_check`, **372,150 gates → padded to 2¹⁹**.
  12 threads.
- **WHIR:** `p3-whir`, single polynomial of size **2¹⁹**, our cross-field bench (BabyBear/KoalaBear/
  Goldilocks). 12 cores.

## ⚠️ What is and isn't comparable
bb measures a **full proof** of a 372k-gate circuit: 31 committed polynomials + sumcheck + a single
batched KZG opening. WHIR here is a **single-polynomial PCS** (commit / open / verify). So:

- **Cleanly comparable (same machine, same 2¹⁹ size):** per-polynomial **commit**, **proof size**, **verify**.
- **Not directly comparable:** bb "total prove" (whole protocol, 31 polys) vs WHIR single-poly open;
  and KZG's *batched* opening of 31 polys vs WHIR opening 1.
- **Mechanism differs:** KZG (pairing, trusted CRS, MSM) vs WHIR (hash/FRI, transparent, Merkle).

## Headline — per-polynomial commit @2¹⁹ (same M3 Pro)
| | commit / polynomial |
|---|--:|
| **bb — KZG (MSM)** | **28.7 ms** (890.8 ms ÷ 31) |
| WHIR — **BabyBear** | **28.7 ms** |
| WHIR — **KoalaBear** | **26.2 ms** |
| WHIR — Goldilocks | 197 ms |

**WHIR's Merkle commit over a 31-bit field is dead even with — or faster than — KZG's MSM commit, per
polynomial, on the identical machine.** (Goldilocks' 64-bit base + w8 Poseidon2 is the outlier, ~7× slower.)

> Likely (unmeasured) WHIR edge: WHIR commits an entire multi-column witness as **one** Merkle tree,
> whereas bb does 31 separate KZG commits (890 ms total). A single WHIR commit of a 31-wide 2¹⁹ matrix
> would almost certainly beat 31 × 28.7 ms. Our bench only commits 1 column, so this isn't claimed.

## Proof size — the decisive KZG win (machine-independent)
| | proof size @2¹⁹ |
|---|--:|
| **bb — KZG** | **15.9 KiB** (constant in circuit size) |
| WHIR BabyBear 100-bit | ~111 KiB |
| WHIR BabyBear 128-bit | ~147 KiB |
| WHIR Goldilocks 128-bit | ~161 KiB |

→ **KZG proof is ~7–10× smaller.** This is the core FRI-vs-KZG tradeoff and KZG's main advantage.

## Verify
| | verify @2¹⁹ |
|---|--:|
| bb — KZG | ~30 ms wall (≈25 ms is process/CRS startup; the pairing is ~ms) |
| WHIR — BabyBear | ~2.4 ms (s100) / 3.2 ms (s128), in-process |
| WHIR — KoalaBear | ~2.0 / 2.6 ms |

Both are cheap. KZG verify is constant-size (2 pairings); WHIR verify grows slowly (queries × Merkle
path). In-process they're the same order of magnitude.

## Full bb prove breakdown (M3 Pro, 2¹⁹, for reference)
`CircuitProve` **2.95 s** (total prove 2.99 s, peak **1.07 GiB**):

| stage | time | note |
|---|--:|---|
| `CommitmentKey::commit` ×31 | 890.8 ms | the PCS commit (28.7 ms/poly) |
| `OinkProver::prove` | 1010 ms | witness rounds (incl. wire/z_perm/lookup commits) |
| PCS open (Gemini/Shplonk/KZG, unlabeled remainder) | ~340 ms | batched opening of all 31 |
| `sumcheck.prove` | 274 ms | 19 rounds |
| `create_circuit` + `ProverInstance` | ~394 ms | ACIR→circuit + trace populate |

bb preprocessing (`write_vk`): ~0.53 s. (EPYC published: prove 3.19 s, peak 713 MiB — M3 prove is
marginally faster but uses more memory.)

## Summary
| | bb / KZG (UltraHonk) | WHIR (31-bit fields) |
|---|---|---|
| Commit / poly @2¹⁹ | 28.7 ms | **26–29 ms** (tie) |
| Proof size | **15.9 KiB** | 111–161 KiB |
| Verify (in-process) | ~ms (pairing) | 2–3 ms |
| Setup | trusted CRS | **transparent** |
| Field | **BN254 native** | small fields only (no BN254) |
| Post-quantum | no | **plausibly** (hash-based) |

**Takeaway:** on the same machine, WHIR's commit throughput **matches KZG** per polynomial (KoalaBear
beats it), and WHIR adds transparent setup + plausible post-quantum security — but pays **~8× in proof
size**. KZG keeps constant-size proofs and native BN254. They are not substitutable on the same curve:
WHIR cannot run over BN254 (it needs `PrimeField64`), so a true *same-field* WHIR-vs-KZG is impossible —
this comparison is same-machine + same polynomial size, different field and mechanism.

## Reproduce the bb side
```bash
gh repo clone suyash67/noir-rsa-passport-bench
noirup -v 1.0.0-beta.19 && bbup -v 4.0.0-nightly.20260120
cd noir-rsa-passport-bench/noir-passport-monolithic/complete_age_check
nargo compile && nargo execute witness
bb write_vk -b target/complete_age_check.json -o out
BB_BENCH=1 bb prove -b target/complete_age_check.json -w target/witness.gz \
  -k out/vk -o out --print_bench --bench_out_hierarchical out/bench.json -v
bb verify -k out/vk -p out/proof -i out/public_inputs
```
