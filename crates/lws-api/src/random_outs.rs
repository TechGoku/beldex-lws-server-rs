//! Decoy selection for `get_random_outs`: the gamma distribution for ringct
//! outputs (from "An Empirical Analysis of Traceability in the Monero
//! Blockchain"), a triangular distribution for pre-ringct amounts, and a
//! separate gamma bucket for HF22 privacy tokens, whose rings must be drawn
//! from token outputs only.

use rand::Rng;
use rand_distr::{Distribution as _, Gamma};
use serde_json::{json, Value as Json};

use crate::daemon_data::DaemonData;
use crate::error::{ApiError, ApiResult};
use crate::wire::hex32;

const GAMMA_SHAPE: f64 = 19.28;
const GAMMA_SCALE: f64 = 1.0 / 1.61;
const TARGET_BLOCK_TIME: u64 = 30;
const BLOCKS_IN_A_YEAR: usize = (86_400 / TARGET_BLOCK_TIME as usize) * 365;
const SPENDABLE_AGE: usize = 2;
const DEFAULT_UNLOCK_TIME: f64 = (SPENDABLE_AGE as u64 * TARGET_BLOCK_TIME) as f64;
const RECENT_SPEND_WINDOW: u64 = 2 * TARGET_BLOCK_TIME;

pub const MAX_RING_SIZE: u32 = 50;
pub const MAX_RINGS: usize = 200;

#[derive(Debug)]
struct NotEnoughMixin;

pub struct GammaPicker {
    offsets: Vec<u64>,
    /// Rank -> global index, for buckets (tokens) whose ranks are not global ids.
    bucket_to_global: Vec<u64>,
    outputs_per_second: f64,
    gamma: Gamma<f64>,
}

impl GammaPicker {
    pub fn new(offsets: Vec<u64>, bucket_to_global: Vec<u64>) -> GammaPicker {
        let mut outputs_per_second = 0.0;
        if let Some(&last) = offsets.last() {
            let blocks = offsets.len().min(BLOCKS_IN_A_YEAR);
            let initial = if blocks < offsets.len() { offsets[offsets.len() - blocks - 1] } else { 0 };
            outputs_per_second = (last - initial) as f64 / (TARGET_BLOCK_TIME as f64 * blocks as f64);
        }
        GammaPicker {
            offsets,
            bucket_to_global,
            outputs_per_second,
            gamma: Gamma::new(GAMMA_SHAPE, GAMMA_SCALE).expect("valid gamma parameters"),
        }
    }

    fn is_valid(&self) -> bool {
        self.offsets.len() >= SPENDABLE_AGE
    }

    fn spendable_upper_bound(&self) -> u64 {
        if self.is_valid() {
            self.offsets[self.offsets.len() - SPENDABLE_AGE]
        } else {
            0
        }
    }

    fn to_global(&self, rank: u64) -> Option<u64> {
        if self.bucket_to_global.is_empty() {
            Some(rank)
        } else {
            self.bucket_to_global.get(rank as usize).copied()
        }
    }

    fn pick(&self, rng: &mut impl Rng) -> Result<u64, NotEnoughMixin> {
        if !self.is_valid() {
            return Err(NotEnoughMixin);
        }
        let end = self.offsets.len() - SPENDABLE_AGE + 1;
        let spendable = self.spendable_upper_bound();
        for _ in 0..100 {
            let mut age = self.gamma.sample(rng).exp();
            if age > DEFAULT_UNLOCK_TIME {
                age -= DEFAULT_UNLOCK_TIME;
            } else {
                age = rng.random_range(0..RECENT_SPEND_WINDOW) as f64;
            }
            let index = (age * self.outputs_per_second) as u64;
            if index >= spendable {
                continue;
            }
            let index = spendable - 1 - index;
            let selection = self.offsets[..end].partition_point(|&o| o < index);
            if selection == end {
                continue;
            }
            let first = if selection == 0 { 0 } else { self.offsets[selection - 1] };
            let n = self.offsets[selection] - first;
            if n != 0 {
                if let Some(global) = self.to_global(first + rng.random_range(0..n)) {
                    return Ok(global);
                }
            }
        }
        Err(NotEnoughMixin)
    }

    fn pick_n(&self, n: usize, rng: &mut impl Rng) -> Result<Vec<u64>, NotEnoughMixin> {
        let spendable = self.spendable_upper_bound() as usize;
        if !self.is_valid() || spendable < n {
            return Err(NotEnoughMixin);
        }
        if spendable == n {
            return (0..n as u64).map(|r| self.to_global(r).ok_or(NotEnoughMixin)).collect();
        }
        (0..n).map(|_| self.pick(rng)).collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Histogram {
    pub amount: u64,
    pub total: u64,
    pub unlocked: u64,
}

fn triangular(hist: &Histogram, n: usize, rng: &mut impl Rng) -> Result<Vec<u64>, NotEnoughMixin> {
    if hist.unlocked < n as u64 {
        return Err(NotEnoughMixin);
    }
    if hist.unlocked == n as u64 {
        return Ok((0..n as u64).collect());
    }
    const MAX: u64 = 1 << 53;
    Ok((0..n)
        .map(|_| loop {
            let frac = ((rng.random::<u64>() % MAX) as f64 / MAX as f64).sqrt();
            let index = (frac * hist.total as f64) as u64;
            if index < hist.unlocked {
                break index;
            }
        })
        .collect())
}

#[derive(Clone, Debug)]
pub struct Decoy {
    pub global_index: u64,
    pub key: [u8; 32],
    pub mask: [u8; 32],
    pub blinded_token_id: Option<[u8; 32]>,
}

#[derive(Clone, Debug)]
pub struct Ring {
    pub amount: u64,
    pub is_token: bool,
    pub outputs: Vec<Decoy>,
}

struct FetchedKey {
    key: [u8; 32],
    mask: [u8; 32],
    unlocked: bool,
    blinded_token_id: Option<[u8; 32]>,
}

async fn fetch_keys(data: &DaemonData, indices: &[u64]) -> ApiResult<Vec<FetchedKey>> {
    let r = data.daemon.json_rpc("get_outs", json!({"output_indices": indices, "get_txid": false})).await?;
    let outs =
        r.get("outs").and_then(Json::as_array).ok_or_else(|| ApiError::Unavailable("get_outs: no outs".into()))?;
    outs.iter()
        .map(|o| {
            let hex = |k: &str| o.get(k).and_then(Json::as_str).and_then(hex32);
            Ok(FetchedKey {
                key: hex("key").ok_or_else(|| ApiError::Unavailable("get_outs: bad key".into()))?,
                mask: hex("mask").ok_or_else(|| ApiError::Unavailable("get_outs: bad mask".into()))?,
                unlocked: o.get("unlocked").and_then(Json::as_bool).unwrap_or(false),
                blinded_token_id: hex("blinded_token_id").filter(|k| *k != [0u8; 32]),
            })
        })
        .collect()
}

/// Builds `mixin` decoys for each requested ring. Rings that cannot be filled
/// are dropped, matching the reference server.
pub async fn pick(data: &DaemonData, mixin: u32, requests: Vec<(u64, bool)>) -> ApiResult<Vec<Ring>> {
    let mixin = mixin as usize;
    if mixin == 0 || requests.is_empty() {
        return Ok(requests.iter().map(|_| Ring { amount: 0, is_token: false, outputs: vec![] }).collect());
    }

    let mut rings: Vec<Ring> =
        requests.iter().map(|&(amount, is_token)| Ring { amount, is_token, outputs: vec![] }).collect();
    // Non-zero amounts first, as the reference orders them; stable, so the
    // ringct rings keep the caller's order (and their token flags).
    rings.sort_by_key(|r| std::cmp::Reverse(r.amount));

    let mut histograms = Vec::new();
    let legacy: Vec<u64> = rings.iter().map(|r| r.amount).filter(|&a| a != 0).collect();
    if !legacy.is_empty() {
        let r = data
            .daemon
            .json_rpc(
                "get_output_histogram",
                json!({"amounts": legacy, "min_count": 0, "max_count": 0, "unlocked": true, "recent_cutoff": 0}),
            )
            .await?;
        for h in r.get("histogram").and_then(Json::as_array).into_iter().flatten() {
            let n = |k: &str| h.get(k).and_then(Json::as_u64).unwrap_or(0);
            histograms.push(Histogram {
                amount: n("amount"),
                total: n("total_instances"),
                unlocked: n("unlocked_instances"),
            });
        }
    }

    let any_ringct = rings.iter().any(|r| r.amount == 0);
    let any_token = rings.iter().any(|r| r.is_token && r.amount == 0);
    let (native, token) = if any_ringct {
        let d = data.distribution().await?;
        let native = GammaPicker::new(d.native.clone(), Vec::new());
        let token = d.token.as_ref().map(|(dist, idx)| GammaPicker::new(dist.clone(), idx.clone()));
        if any_token && token.as_ref().is_none_or(|t| t.bucket_to_global.is_empty()) {
            return Err(ApiError::BadRequest("not enough token outputs to build a ring".into()));
        }
        (Some(native), token)
    } else {
        (None, None)
    };

    for _attempt in 0..64 {
        let mut proposed: Vec<(usize, u64)> = Vec::new();
        let mut keep = Vec::with_capacity(rings.len());
        {
            let mut rng = rand::rng();
            for (i, ring) in rings.iter_mut().enumerate() {
                let have: std::collections::HashSet<u64> = ring.outputs.iter().map(|o| o.global_index).collect();
                let missing = mixin.saturating_sub(ring.outputs.len());
                if missing == 0 {
                    keep.push(true);
                    continue;
                }
                let picked = if ring.amount == 0 {
                    let picker = if ring.is_token { token.as_ref() } else { native.as_ref() };
                    picker.ok_or(NotEnoughMixin).and_then(|p| p.pick_n(missing, &mut rng))
                } else {
                    match histograms.iter().find(|h| h.amount == ring.amount) {
                        Some(h) => triangular(h, missing, &mut rng),
                        None => Err(NotEnoughMixin),
                    }
                };
                match picked {
                    Ok(mut indices) => {
                        indices.sort_unstable();
                        indices.dedup();
                        indices.retain(|g| !have.contains(g));
                        proposed.extend(indices.into_iter().map(|g| (i, g)));
                        keep.push(true);
                    }
                    Err(NotEnoughMixin) => keep.push(false),
                }
            }
        }
        // Drop rings that cannot be filled, re-indexing the proposals.
        let mut remap = Vec::with_capacity(rings.len());
        let mut next = 0;
        for k in &keep {
            remap.push(if *k { Some(next) } else { None });
            next += usize::from(*k);
        }
        let mut k = keep.iter();
        rings.retain(|_| *k.next().unwrap());
        let proposed: Vec<(usize, u64)> = proposed.into_iter().filter_map(|(i, g)| remap[i].map(|j| (j, g))).collect();
        if rings.is_empty() {
            return Ok(rings);
        }

        if !proposed.is_empty() {
            let indices: Vec<u64> = proposed.iter().map(|&(_, g)| g).collect();
            let keys = fetch_keys(data, &indices).await?;
            if keys.len() != indices.len() {
                return Err(ApiError::Unavailable("get_outs returned the wrong number of outputs".into()));
            }
            for ((ring, global_index), key) in proposed.into_iter().zip(keys) {
                if key.unlocked {
                    rings[ring].outputs.push(Decoy {
                        global_index,
                        key: key.key,
                        mask: key.mask,
                        blinded_token_id: key.blinded_token_id,
                    });
                }
            }
        }
        if rings.iter().all(|r| r.outputs.len() >= mixin) {
            for r in &mut rings {
                r.outputs.sort_by_key(|o| o.global_index);
                r.outputs.truncate(mixin);
            }
            return Ok(rings);
        }
    }
    Err(ApiError::Unavailable("could not find enough unlocked decoys".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(blocks: usize, per_block: u64) -> Vec<u64> {
        (1..=blocks as u64).map(|b| b * per_block).collect()
    }

    #[test]
    fn gamma_picks_spendable_outputs() {
        let picker = GammaPicker::new(chain(10_000, 20), Vec::new());
        let bound = picker.spendable_upper_bound();
        let mut rng = rand::rng();
        for _ in 0..2_000 {
            let g = picker.pick(&mut rng).unwrap();
            assert!(g < bound, "{g} >= {bound}");
        }
    }

    #[test]
    fn token_ranks_map_to_global_indices() {
        // Three token outputs at global indices 100, 250, 900.
        let picker = GammaPicker::new(vec![0, 1, 2, 3, 3, 3], vec![100, 250, 900]);
        let mut rng = rand::rng();
        let all = picker.pick_n(3, &mut rng).unwrap();
        assert_eq!(all, vec![100, 250, 900]);
        assert!(picker.pick_n(4, &mut rng).is_err());
    }

    #[test]
    fn small_chains_are_refused() {
        let picker = GammaPicker::new(vec![5], Vec::new());
        assert!(picker.pick_n(1, &mut rand::rng()).is_err());
    }

    #[test]
    fn triangular_stays_unlocked() {
        let h = Histogram { amount: 5, total: 1_000, unlocked: 900 };
        let mut rng = rand::rng();
        assert!(triangular(&h, 50, &mut rng).unwrap().iter().all(|&i| i < 900));
        assert_eq!(triangular(&Histogram { unlocked: 3, ..h }, 3, &mut rng).unwrap(), vec![0, 1, 2]);
        assert!(triangular(&Histogram { unlocked: 2, ..h }, 3, &mut rng).is_err());
    }
}
