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
