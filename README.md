# # Yield-Sensitive DLMM - (yield_sensitive_dlmm)



## 🚀 Overview

yield_sensitive_dlmm implements a yield‑sensitive dynamic‑liquidity market maker (DLMM) with an on‑chain limit orderbook. The program dynamically computes concentrated‑liquidity bands (price buckets) that adapt to yield signals and spot price, supports LP deposits/withdrawals, on‑chain orderbook matching, governance, and keeper bounties.
This is a proof-of-concept developed in Solana Playground for research purposes.  Yield-Sensitive DLMM dynamically computes concentrated-liquidity bands based on yield signals and spot price, and includes an on-chain limit orderbook for hybrid routing and matching.


---


## ✅ Key Features

- **Yield-sensitive bands:** center & width adapt to EMAs of yields and spot price.  
- **Hysteresis & circuit breakers:** prevents noisy or large sudden updates.  
- **Dynamic fees:** volatility-driven fee adjustments with maker/taker split knobs.  
- **Hybrid routing:** `BookFirst` or `DlmmFirst` routing modes for orders.  
- **Multisig admin model:** up to `MAX_ADMINS` with a configurable quorum threshold.  
- **Bounty system:** rewards keepers that call `post_yields_and_update` when meaningful changes occur.  
- **On-chain metrics & band digest:** digest emitted for off-chain verification and monitoring.

---

## 📦 Primary Accounts & Storage

### Pool (Account)
Holds global pool state for a token pair. Important fields:
- `version`, `bump` — layout version + PDA bump.
- Admins: `admins`, `admin_threshold`.
- Assets: `mint_a`, `mint_b`, `vault_a`, `vault_b`, `treasury_a`, `treasury_b`.
- Yield/spot EMAs: `ema_y_a_bps`, `ema_y_b_bps`, `ema_spot_1e6`.
- Band params: `n_bands`, `base_width_bps`, `width_slope_per_kbps`, `bias_per_kbps`, `decay_per_band_bps`.
- Fee params: `fee_base_bps`, `fee_k_per_bps`, `fee_max_bps`, `fee_current_bps`.
- Routing / STP: `stp_mode`, `route_mode`.
- `bands: Vec<Band>` — runtime list of `Band` structs.

### Band
Per-band data and runtime accounting:
- `lower_price_1e6`, `upper_price_1e6`, `weight_bps`.
- Fee accumulators: `fee_growth_a_1e18`, `fee_growth_b_1e18`.
- Reserves & shares: `reserves_a`, `reserves_b`, `total_shares`.
- `is_active` flag.

### Position
LP receipt for a deposit into a band:
- `owner`, `band_idx`, `shares`, `last_fee_growth_*`, `receipt_nonce`, `min_unlock_slot`, `approved`.

### OrderBook
Per-pool on-chain book with `bids`, `asks` (vectors of `PriceLevel`) and `event_q` (book events queue).

### MetricsRing
Circular buffer storing recent `MetricItem` entries: `slot`, `center_price_1e6`, `width_bps`, `hash`.

---

## 🔧 Core Instructions (high-level)

### `initialize_pool(ctx, p: InitParamsV3)`
Creates a v3 `Pool` PDA and associated vaults/treasuries. Key knobs in `InitParamsV3`:
- Multisig & roles: `admins`, `admin_threshold`, `risk_admin`, `ops_admin`, `fee_admin`.
- Band configuration: `n_bands`, `base_width_bps`, `min/max_width_bps`, `width_slope_per_kbps`, `decay_per_band_bps`.
- EMA alphas & limits: `alpha_y_bps`, `alpha_spot_bps`, `alpha_twap_bps`, `alpha_vol_bps`, `max_twap_dev_bps`.
- Fees & bounty: `fee_base_bps`, `fee_k_per_bps`, `fee_max_bps`, `bounty_rate_microunits`, etc.

`initialize_pool` computes initial bands via `recompute_bands` and validates invariants.

### `post_yields_and_update(ctx, y_a_bps_raw, y_b_bps_raw, spot_price_1e6_raw, cu_price_micro_lamports)`
Keeper/updater entrypoint that:
- Validates caller (updater or admin), optional oracle signer.
- Applies EMAs for yields and spot price and calculates candidate center/width.
- Enforces TWAP deviation guard and hysteresis counters before committing changes.
- Updates volatility EMA and computes dynamic fee (`fee_current_bps`).
- Calls `recompute_bands`, `mark_inactive_by_floor`, `renormalize_active_weights`.
- Pays bounty to caller via `pay_bounty_if_any` and emits `BandsDigestUpdatedV`.

Guards: cooldown slots, min CU price, hysteresis thresholds, TWAP deviation limits.

### Liquidity ops
- `add_liquidity`: deposits A/B into vaults and mints `Position` shares (checks deposit ratio guard).
- `remove_liquidity`: burns shares, transfers proportional reserves from vaults back to user using pool PDA signer.
- `collect_fees`: computes owed fees using `fee_growth_*` deltas and transfers from `treasury_*` to user.

All protocol transfers use `pool_signer_seeds(pool)` as the authority.

### Orderbook ops
- `init_orderbook`: creates `OrderBook` PDA with tick sizing and per-level capacity.
- `place_order`: supports `post_only`, `tif`, `reduce_only`, `client_id` and routes based on `RouteMode`.
- `match_against_book` & `take_from_bands`: matching logic for orderbook and DLMM band liquidity.
- `crank_match`: loops to clear crossable top levels (useful for matchers / crankers).

---

