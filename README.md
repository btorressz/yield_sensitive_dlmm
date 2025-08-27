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
