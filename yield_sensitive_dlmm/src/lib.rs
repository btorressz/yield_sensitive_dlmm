use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use solana_program::keccak;

declare_id!("ebvdBEBKz6UK1Xs9mnGs7TsR2vgKyPP2idaFEqGRTRQ");

/* =============================================================================
                                   Program
============================================================================= */

#[program]
pub mod yield_sensitive_dlmm {
    use super::*;

    /* ---------------------------- Init / Migrate --------------------------- */

    pub fn initialize_pool(ctx: Context<InitializePool>, p: InitParamsV3) -> Result<()> {
        // capture pool key before any mutable borrow usage
        let pool_key = ctx.accounts.pool.key();

        let pool = &mut ctx.accounts.pool;
        pool.version = 3;
        pool.bump = ctx.bumps.pool;

        // roles
        pool.admin_threshold = p.admin_threshold.max(1).min(MAX_ADMINS as u8);
        pool.admins = p.admins;
        require!(
            count_nonzero_admins(&pool.admins) >= pool.admin_threshold as usize,
            DlmmError::BadMultisig
        );
        pool.risk_admin = p.risk_admin;
        pool.ops_admin = p.ops_admin;
        pool.fee_admin = p.fee_admin;

        pool.mint_a = ctx.accounts.mint_a.key();
        pool.mint_b = ctx.accounts.mint_b.key();
        pool.vault_a = ctx.accounts.vault_a.key();
        pool.vault_b = ctx.accounts.vault_b.key();
        pool.treasury_a = ctx.accounts.treasury_a.key();
        pool.treasury_b = ctx.accounts.treasury_b.key();

        // updater/oracle
        pool.updater = p.updater;
        pool.oracle_signer = p.oracle_signer;

        // widths / bands
        require!(p.n_bands > 0 && (p.n_bands as usize) <= MAX_BANDS, DlmmError::InvalidNBands);
        pool.n_bands = p.n_bands;
        pool.base_width_bps = p.base_width_bps;
        pool.min_width_bps = p.min_width_bps;
        pool.max_width_bps = p.max_width_bps;
        pool.width_slope_per_kbps = p.width_slope_per_kbps;
        pool.bias_per_kbps = p.bias_per_kbps;
        pool.decay_per_band_bps = p.decay_per_band_bps;

        // cooldown / circuit breakers
        pool.max_center_move_bps = p.max_center_move_bps;
        pool.max_width_change_bps = p.max_width_change_bps;
        pool.max_weight_shift_bps = p.max_weight_shift_bps;
        pool.min_update_interval_slots = p.min_update_interval_slots;
        pool.last_update_slot = 0;

        // hysteresis
        pool.hyst_center_bps = p.hyst_center_bps;
        pool.hyst_width_bps = p.hyst_width_bps;
        pool.hyst_required_n = p.hyst_required_n.max(1);
        pool.hyst_ctr_center = 0;
        pool.hyst_ctr_width = 0;

        // EMA / TWAP / volatility & fees
        pool.y_a_bps = p.initial_y_a_bps;
        pool.y_b_bps = p.initial_y_b_bps;
        pool.spot_price_1e6 = p.initial_spot_price_1e6;
        pool.ema_y_a_bps = p.initial_y_a_bps;
        pool.ema_y_b_bps = p.initial_y_b_bps;
        pool.ema_spot_1e6 = p.initial_spot_price_1e6;
        pool.twap_center_1e6 = p.initial_spot_price_1e6;
        pool.alpha_y_bps = p.alpha_y_bps.max(1).min(10_000);
        pool.alpha_spot_bps = p.alpha_spot_bps.max(1).min(10_000);
        pool.alpha_twap_bps = p.alpha_twap_bps.max(1).min(10_000);
        pool.alpha_vol_bps = p.alpha_vol_bps.max(1).min(10_000);
        pool.max_twap_dev_bps = p.max_twap_dev_bps;
        pool.vol_ema_bps = 0;
        pool.fee_base_bps = p.fee_base_bps;
        pool.fee_k_per_bps = p.fee_k_per_bps;
        pool.fee_max_bps = p.fee_max_bps;
        pool.fee_current_bps = p.fee_base_bps;

        // fee schedule knobs (CLOB split)
        pool.maker_rebate_max_bps = p.maker_rebate_max_bps;
        pool.taker_min_bps = p.taker_min_bps;
        pool.stp_mode = p.stp_mode as u8;
        pool.route_mode = p.route_mode as u8;

        // deposit ratio guard
        pool.deposit_ratio_min_bps = p.deposit_ratio_min_bps;
        pool.deposit_ratio_max_bps = p.deposit_ratio_max_bps;

        // inactivity floor
        pool.inactive_floor_a = p.inactive_floor_a;
        pool.inactive_floor_b = p.inactive_floor_b;

        // bounties / stale alert / priority fee floor
        pool.bounty_rate_microunits = p.bounty_rate_microunits;
        pool.bounty_max = p.bounty_max;
        pool.stale_slots_for_boost = p.stale_slots_for_boost;
        pool.bounty_boost_bps = p.bounty_boost_bps;
        pool.min_cu_price = p.min_cu_price;
        pool.needs_update = false;

        // flags & governance
        pool.is_paused = false;
        pool.pause_bands = false;
        pool.pause_deposits = false;
        pool.pause_withdraws = false;
        pool.pause_orderbook = false;
        pool.post_only_until_slot = 0;
        pool.g_pending = None;

        // top-of-book cache
        pool.best_bid_1e6 = 0;
        pool.best_ask_1e6 = u64::MAX;
        pool.book_depth_bps = 0;

        // initial bands
        recompute_bands(pool, /*enforce_cb=*/false, /*weights_only=*/false)?;
        assert_invariants(
            &ctx.accounts.mint_a,
            &ctx.accounts.mint_b,
            &ctx.accounts.vault_a,
            &ctx.accounts.vault_b,
            &*pool,
        )?;

        emit!(PoolInitializedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            n_bands: pool.n_bands,
            center_price_1e6: pool.last_center_price_1e6,
            width_bps: pool.last_width_bps,
        });
        Ok(())
    }

/// Per-pool migration: bring an existing Pool account up to the v3 layout/semantics.
/// Idempotent and admin-gated. Captures the pool key before taking a mutable borrow
/// to avoid borrow checker errors when emitting events that reference the key.
pub fn migrate_pool_versions(ctx: Context<AdminScoped>) -> Result<()> {
    // capture immutable values BEFORE the mutable borrow
    let pool_key = ctx.accounts.pool.key();

    // Now take the mutable borrow for updates
    let pool = &mut ctx.accounts.pool;

    require!(pool.version < 3, DlmmError::AlreadyMigrated);

    let from_version = pool.version;
    pool.version = 3;

    if pool.alpha_twap_bps == 0 {
        pool.alpha_twap_bps = 500; // 5%
    }
    if pool.max_twap_dev_bps == 0 {
        pool.max_twap_dev_bps = 500; // 5%
    }

    // If you need derived state refreshed
    recompute_bands(pool, /*enforce_cb=*/false, /*weights_only=*/false)?;

    // safe to use pool_key (captured earlier) in event
    let now = Clock::get()?.slot;
    emit!(PoolMigratedV {
        event_version: EVENT_VERSION,
        pool: pool_key,
        from: from_version,
        to: pool.version,
        migrated_at_slot: now,
    });

    Ok(())
}

    /* ----------------------- Governance (timelock/quorum) ------------------- */

    pub fn propose_params(
        ctx: Context<AdminMultisig>,
        new: SettableParamsV3,
        queue_delay_slots: u64,
        execute_deadline_slots: u64,
    ) -> Result<()> {
        let pool_key = ctx.accounts.pool.key();
        let pool = &mut ctx.accounts.pool;
        require!(
            is_quorum(&pool.admins, pool.admin_threshold, &ctx.remaining_accounts)?,
            DlmmError::BadMultisig
        );
        require!(pool.g_pending.is_none(), DlmmError::ProposalExists);

        let now = Clock::get()?.slot;
        pool.g_pending = Some(GovProposal {
            new,
            queued_at: now,
            earliest_exec: now.saturating_add(queue_delay_slots),
            deadline: now
                .saturating_add(queue_delay_slots)
                .saturating_add(execute_deadline_slots),
            executed: false,
        });
        emit!(ParamsProposedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            queued_at: now,
        });
        Ok(())
    }

    pub fn execute_params(ctx: Context<AdminMultisig>) -> Result<()> {
        let pool_key = ctx.accounts.pool.key();
        let pool = &mut ctx.accounts.pool;
        require!(
            is_quorum(&pool.admins, pool.admin_threshold, &ctx.remaining_accounts)?,
            DlmmError::BadMultisig
        );

        let now = Clock::get()?.slot;
        let mut gp = pool.g_pending.take().ok_or(DlmmError::NoPendingParams)?;
        require!(!gp.executed, DlmmError::ProposalExecuted);
        require!(now >= gp.earliest_exec && now <= gp.deadline, DlmmError::WindowClosed);

        apply_settable_params(pool, &gp.new)?;
        gp.executed = true;
        pool.g_pending = Some(gp);

        // refresh derived state
        recompute_bands(pool, /*enforce_cb=*/false, /*weights_only=*/false)?;
        emit!(ParamsExecutedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            executed_at: now,
        });
        Ok(())
    }

    pub fn set_roles(
        ctx: Context<AdminMultisig>,
        risk_admin: Pubkey,
        ops_admin: Pubkey,
        fee_admin: Pubkey,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        require!(
            is_quorum(&pool.admins, pool.admin_threshold, &ctx.remaining_accounts)?,
            DlmmError::BadMultisig
        );
        pool.risk_admin = risk_admin;
        pool.ops_admin = ops_admin;
        pool.fee_admin = fee_admin;
        Ok(())
    }

    /* ----------------------------- Ops / Risk -------------------------------- */

    pub fn set_pause(ctx: Context<RiskScoped>, flags: PauseFlags) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        if let Some(v) = flags.is_paused {
            pool.is_paused = v;
        }
        if let Some(v) = flags.pause_bands {
            pool.pause_bands = v;
        }
        if let Some(v) = flags.pause_deposits {
            pool.pause_deposits = v;
        }
        if let Some(v) = flags.pause_withdraws {
            pool.pause_withdraws = v;
        }
        if let Some(v) = flags.pause_orderbook {
            pool.pause_orderbook = v;
        }
        Ok(())
    }

    pub fn set_updater(ctx: Context<OpsScoped>, updater: Pubkey, oracle: Option<Pubkey>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.updater = updater;
        pool.oracle_signer = oracle;
        Ok(())
    }

    pub fn emergency_drain(ctx: Context<RiskScoped>, from_a: bool, amount: u64) -> Result<()> {
        // capture pool info before mutable borrow
        let pool_key = ctx.accounts.pool.key();
        let pool_ai = ctx.accounts.pool.to_account_info();

        let pool = &ctx.accounts.pool; // read-only required
        require!(pool.is_paused, DlmmError::NotPaused);

        let seeds = pool_signer_seeds(pool);
        let signer = &[&seeds[..]];
        let (from, to) = if from_a {
            (
                ctx.accounts.vault_a.to_account_info(),
                ctx.accounts.treasury_a.to_account_info(),
            )
        } else {
            (
                ctx.accounts.vault_b.to_account_info(),
                ctx.accounts.treasury_b.to_account_info(),
            )
        };
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer { from, to, authority: pool_ai.clone() },
                signer,
            ),
            amount,
        )?;
        emit!(EmergencyDrainV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            amount,
        });
        Ok(())
    }

    pub fn propose_mint_rotation(
        ctx: Context<RiskScoped>,
        new_mint_a: Option<Pubkey>,
        new_mint_b: Option<Pubkey>,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.proposed_mint_a = new_mint_a;
        pool.proposed_mint_b = new_mint_b;
        Ok(())
    }

    pub fn accept_mint_rotation(ctx: Context<RiskScoped>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        if let Some(m) = pool.proposed_mint_a {
            pool.mint_a = m;
        }
        if let Some(m) = pool.proposed_mint_b {
            pool.mint_b = m;
        }
        pool.proposed_mint_a = None;
        pool.proposed_mint_b = None;
        Ok(())
    }

    /* ------------------------- Updates (keepers) ---------------------------- */

    pub fn post_yields_and_update(
        ctx: Context<PostYieldsAndUpdate>,
        y_a_bps_raw: u16,
        y_b_bps_raw: u16,
        spot_price_1e6_raw: u64,
        cu_price_micro_lamports: u64,
    ) -> Result<()> {
        // capture pool key before mut borrow
        let pool_key = ctx.accounts.pool.key();

        let pool = &mut ctx.accounts.pool;

        // gate: paused
        require!(!pool.is_paused && !pool.pause_bands, DlmmError::Paused);

        // gate: updater + optional oracle
        require!(
            ctx.accounts.caller.key() == pool.updater || is_admin(&pool.admins, &ctx.accounts.caller.key()),
            DlmmError::Unauthorized
        );
        if let Some(oracle_pk) = pool.oracle_signer {
            let o = ctx
                .accounts
                .oracle_signer_opt
                .as_ref()
                .ok_or(DlmmError::MissingOracleSigner)?;
            require!(o.is_signer && o.key() == oracle_pk, DlmmError::Unauthorized);
        }

        // Validate metrics (runtime check)
        if let Some(m) = &ctx.accounts.metrics {
            require!(m.pool == pool_key, DlmmError::Unauthorized);
        }

        // priority fee gate
        if pool.min_cu_price > 0 {
            require!(cu_price_micro_lamports >= pool.min_cu_price, DlmmError::CuPriceTooLow);
        }

        // cooldown (reject)
        let now_slot = Clock::get()?.slot;
        if pool.last_update_slot + (pool.min_update_interval_slots as u64) > now_slot {
            return err!(DlmmError::CooldownNotElapsed);
        }

        // stale-boost flag (computed before updating last_update_slot)
        let stale = now_slot.saturating_sub(pool.last_update_slot);
        pool.needs_update = stale > pool.stale_slots_for_boost;

        // update raw + EMA
        pool.y_a_bps = y_a_bps_raw;
        pool.y_b_bps = y_b_bps_raw;
        pool.spot_price_1e6 = spot_price_1e6_raw;
        pool.ema_y_a_bps = ema_step_u16(pool.ema_y_a_bps, y_a_bps_raw, pool.alpha_y_bps)?;
        pool.ema_y_b_bps = ema_step_u16(pool.ema_y_b_bps, y_b_bps_raw, pool.alpha_y_bps)?;
        let prev_center = pool.last_center_price_1e6;
        pool.ema_spot_1e6 = ema_step_u64(pool.ema_spot_1e6, spot_price_1e6_raw, pool.alpha_spot_bps)?;

        // candidate recompute just to get deltas (no commit yet)
        let (cand_center, cand_width_bps) = preview_center_width(pool)?;
        let d_center_bps = diff_bps_u64(prev_center, cand_center)?;
        let d_width_bps = diff_i_bps(pool.last_width_bps as i32, cand_width_bps as i32)? as u64;

        // TWAP deviation guard
        pool.twap_center_1e6 = ema_step_u64(pool.twap_center_1e6, prev_center.max(1), pool.alpha_twap_bps)?;
        let dev_vs_twap_bps = diff_bps_u64(pool.twap_center_1e6, cand_center)?;
        require!(dev_vs_twap_bps <= pool.max_twap_dev_bps as u64, DlmmError::DeviationTooHigh);

        // hysteresis counters
        pool.hyst_ctr_center = if d_center_bps >= pool.hyst_center_bps as u64 {
            pool.hyst_ctr_center.saturating_add(1)
        } else {
            0
        };
        pool.hyst_ctr_width = if d_width_bps >= pool.hyst_width_bps as u64 {
            pool.hyst_ctr_width.saturating_add(1)
        } else {
            0
        };

        require!(
            pool.hyst_ctr_center >= pool.hyst_required_n || pool.hyst_ctr_width >= pool.hyst_required_n,
            DlmmError::HysteresisNotMet
        );

        // volatility EMA & dynamic fee
        pool.vol_ema_bps = ema_step_u16(pool.vol_ema_bps, (d_center_bps as u16).min(u16::MAX), pool.alpha_vol_bps)?;
        let dyn_fee = (pool.fee_base_bps as u32)
            .saturating_add((pool.fee_k_per_bps as u32).saturating_mul(pool.vol_ema_bps as u32))
            .min(pool.fee_max_bps as u32) as u16;
        pool.fee_current_bps = dyn_fee;

        // partial recompute path for tiny changes
        let tiny_center = d_center_bps <= (pool.hyst_center_bps as u64 / 2).max(1);
        let tiny_width = d_width_bps <= (pool.hyst_width_bps as u64 / 2).max(1);
        let weights_only = tiny_center && tiny_width;

        // commit recompute with CBs (+ weights-only option)
        recompute_bands(pool, /*enforce_cb=*/true, weights_only)?;

        // inactive bands: auto-flag by floor and renormalize active weights
        mark_inactive_by_floor(pool);
        renormalize_active_weights(pool)?;

        // diagnostics & flags
        pool.last_update_slot = now_slot;

        // make CLOB post-only for this slot (anti update-then-trade)
        pool.post_only_until_slot = now_slot;

        // assert invariants (safety net)
        assert_invariants(
            &ctx.accounts.mint_a,
            &ctx.accounts.mint_b,
            &ctx.accounts.vault_a,
            &ctx.accounts.vault_b,
            &*pool,
        )?;

        // pay bounty (only if all checks passed)
        // pass the mutable pool account to the function (it will reborrow immutably internally)
        pay_bounty_if_any(
            &ctx.accounts.token_program,
            pool,
            &ctx.accounts.treasury_a,
            &ctx.accounts.treasury_b,
            &ctx.accounts.caller_ata_a,
            &ctx.accounts.caller_ata_b,
            d_center_bps as u32,
            d_width_bps as u32,
        )?;

        // digest + metrics
        emit!(BandsDigestUpdatedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            width_bps: pool.last_width_bps,
            center_price_1e6: pool.last_center_price_1e6,
            total_weight_bps: pool.total_weight_bps,
            hash: digest_bands(&pool.bands),
            slot: now_slot,
            fee_current_bps: pool.fee_current_bps,
            vol_ema_bps: pool.vol_ema_bps,
        });

        if let Some(macc) = ctx.accounts.metrics.as_mut() {
            push_metric(
                &mut *macc,
                now_slot,
                pool.last_center_price_1e6,
                pool.last_width_bps,
                pool.total_weight_bps,
                digest_bands(&pool.bands),
            );
        }

        Ok(())
    }

    /* --------------------------- Liquidity ops ------------------------------ */

    #[allow(clippy::too_many_arguments)]
    pub fn add_liquidity(
        ctx: Context<AddLiquidity>,
        band_idx: u8,
        amount_a: u64,
        amount_b: u64,
        receipt_nonce: u64,
        min_unlock_after_slots: u64,
    ) -> Result<()> {
        // capture immutable info before creating mutable borrow
        let pool_key = ctx.accounts.pool.key();
        let _pool_ai = ctx.accounts.pool.to_account_info(); // captured for safety if needed

        let pool = &mut ctx.accounts.pool;

        require!(!pool.is_paused && !pool.pause_deposits, DlmmError::Paused);
        require!((band_idx as usize) < pool.n_bands as usize, DlmmError::InvalidBandIndex);
        require!(amount_a > 0 || amount_b > 0, DlmmError::ZeroAmount);

        // ratio guard
        require!(
            passes_ratio_guard(pool, amount_a, amount_b)?,
            DlmmError::DepositRatioOutOfBounds
        );

        let b = pool
            .bands
            .get_mut(band_idx as usize)
            .ok_or(DlmmError::InvalidBandIndex)?;
        require!(b.is_active, DlmmError::BandInactive);

        if amount_a > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.user_ata_a.to_account_info(),
                        to: ctx.accounts.vault_a.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                amount_a,
            )?;
        }
        if amount_b > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.user_ata_b.to_account_info(),
                        to: ctx.accounts.vault_b.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                amount_b,
            )?;
        }

        let shares = quote_shares_to_mint(b, amount_a, amount_b);
        require!(shares > 0, DlmmError::ZeroShares);
        b.total_shares = b.total_shares.saturating_add(shares);
        b.reserves_a = b.reserves_a.saturating_add(amount_a);
        b.reserves_b = b.reserves_b.saturating_add(amount_b);
        b.util_a = b.util_a.saturating_add(amount_a);
        b.util_b = b.util_b.saturating_add(amount_b);

        let pos = &mut ctx.accounts.position;
        pos.bump = ctx.bumps.position;
        pos.pool = pool_key;
        pos.owner = ctx.accounts.user.key();
        pos.band_idx = band_idx;
        pos.shares = pos.shares.saturating_add(shares);
        pos.last_fee_growth_a_1e18 = b.fee_growth_a_1e18;
        pos.last_fee_growth_b_1e18 = b.fee_growth_b_1e18;
        pos.receipt_nonce = receipt_nonce;
        pos.min_unlock_slot = Clock::get()?.slot.saturating_add(min_unlock_after_slots);
        pos.approved = None;

        emit!(LiquidityAddedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            owner: pos.owner,
            band_idx,
            shares,
            receipt: ctx.accounts.position.key(),
        });

        // pass &*pool (immutable reborrow) to avoid borrowing ctx.accounts.pool immutably
        assert_invariants(
            &ctx.accounts.mint_a,
            &ctx.accounts.mint_b,
            &ctx.accounts.vault_a,
            &ctx.accounts.vault_b,
            &*pool,
        )?;
        Ok(())
    }

    pub fn remove_liquidity(ctx: Context<RemoveLiquidity>, shares_to_burn: u64, close_position: bool) -> Result<()> {
        // capture immutable info first
        let pool_key = ctx.accounts.pool.key();
        let pool_ai = ctx.accounts.pool.to_account_info();

        let pool = &mut ctx.accounts.pool;

        require!(!pool.is_paused && !pool.pause_withdraws, DlmmError::Paused);

        let pos = &mut ctx.accounts.position;
        let now = Clock::get()?.slot;
        require!(now >= pos.min_unlock_slot, DlmmError::PositionLocked);
        require!(
            pos.owner == ctx.accounts.user.key() || pos.approved == Some(ctx.accounts.user.key()),
            DlmmError::Unauthorized
        );
        require!(shares_to_burn > 0 && shares_to_burn <= pos.shares, DlmmError::InvalidAmount);

        let b = pool
            .bands
            .get_mut(pos.band_idx as usize)
            .ok_or(DlmmError::InvalidBandIndex)?;
        require!(b.total_shares > 0, DlmmError::ZeroShares);

        let out_a =
            (u128::from(b.reserves_a) * u128::from(shares_to_burn) / u128::from(b.total_shares)) as u64;
        let out_b =
            (u128::from(b.reserves_b) * u128::from(shares_to_burn) / u128::from(b.total_shares)) as u64;

        b.total_shares = b.total_shares.saturating_sub(shares_to_burn);
        b.reserves_a = b.reserves_a.saturating_sub(out_a);
        b.reserves_b = b.reserves_b.saturating_sub(out_b);

        let seeds = pool_signer_seeds(&*pool);
        let signer = &[&seeds[..]];
        if out_a > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault_a.to_account_info(),
                        to: ctx.accounts.user_ata_a.to_account_info(),
                        authority: pool_ai.clone(),
                    },
                    signer,
                ),
                out_a,
            )?;
        }
        if out_b > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault_b.to_account_info(),
                        to: ctx.accounts.user_ata_b.to_account_info(),
                        authority: pool_ai.clone(),
                    },
                    signer,
                ),
                out_b,
            )?;
        }

        pos.shares = pos.shares.saturating_sub(shares_to_burn);
        if close_position && pos.shares == 0 {
            // Anchor will close via `close = user`
        }

        emit!(LiquidityRemovedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            owner: pos.owner,
            band_idx: pos.band_idx,
            shares_burned: shares_to_burn,
            out_a,
            out_b,
            receipt: ctx.accounts.position.key(),
        });

        assert_invariants(
            &ctx.accounts.mint_a,
            &ctx.accounts.mint_b,
            &ctx.accounts.vault_a,
            &ctx.accounts.vault_b,
            &*pool,
        )?;
        Ok(())
    }

    pub fn collect_fees(ctx: Context<WithPosition>) -> Result<()> {
        // capture immutable info first
        let pool_key = ctx.accounts.pool.key();
        let pool_ai = ctx.accounts.pool.to_account_info();

        let pool = &mut ctx.accounts.pool;

        let pos = &mut ctx.accounts.position;
        let b = pool
            .bands
            .get_mut(pos.band_idx as usize)
            .ok_or(DlmmError::InvalidBandIndex)?;
        require!(pos.shares > 0 && b.total_shares > 0, DlmmError::ZeroShares);

        let owed_a = mul_div_1e18(
            pos.shares,
            b.fee_growth_a_1e18.saturating_sub(pos.last_fee_growth_a_1e18),
        );
        let owed_b = mul_div_1e18(
            pos.shares,
            b.fee_growth_b_1e18.saturating_sub(pos.last_fee_growth_b_1e18),
        );

        pos.last_fee_growth_a_1e18 = b.fee_growth_a_1e18;
        pos.last_fee_growth_b_1e18 = b.fee_growth_b_1e18;

        let seeds = pool_signer_seeds(&*pool);
        let signer = &[&seeds[..]];

        if owed_a > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.treasury_a.to_account_info(),
                        to: ctx.accounts.user_ata_a.to_account_info(),
                        authority: pool_ai.clone(),
                    },
                    signer,
                ),
                owed_a,
            )?;
        }
        if owed_b > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.treasury_b.to_account_info(),
                        to: ctx.accounts.user_ata_b.to_account_info(),
                        authority: pool_ai.clone(),
                    },
                    signer,
                ),
                owed_b,
            )?;
        }

        emit!(FeesCollectedV {
            event_version: EVENT_VERSION,
            pool: pool_key,
            owner: pos.owner,
            band_idx: pos.band_idx,
            out_a: owed_a,
            out_b: owed_b
        });
        Ok(())
    }

    pub fn approve_position(ctx: Context<WithPosition>, spender: Option<Pubkey>) -> Result<()> {
        let pos = &mut ctx.accounts.position;
        require!(pos.owner == ctx.accounts.user.key(), DlmmError::Unauthorized);
        pos.approved = spender;
        Ok(())
    }

    pub fn transfer_position(ctx: Context<WithPosition>, new_owner: Pubkey) -> Result<()> {
        let pos = &mut ctx.accounts.position;
        require!(
            pos.owner == ctx.accounts.user.key() || pos.approved == Some(ctx.accounts.user.key()),
            DlmmError::Unauthorized
        );
        pos.owner = new_owner;
        pos.approved = None;
        Ok(())
    }

    pub fn recenter_compact(ctx: Context<RiskScoped>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        mark_inactive_by_floor(pool);
        compact_active_bands(pool);
        renormalize_active_weights(pool)?;
        Ok(())
    }

    /* --------------------------- Orderbook: init ---------------------------- */

    pub fn init_orderbook(ctx: Context<InitOrderBook>, tick_1e6: u64, max_levels: u16) -> Result<()> {
        let pool = &ctx.accounts.pool;
        require!(!pool.pause_orderbook, DlmmError::Paused);
        require!(tick_1e6 > 0, DlmmError::ParamOutOfRange);

        let ob = &mut ctx.accounts.orderbook;
        ob.bump = ctx.bumps.orderbook;
        ob.pool = ctx.accounts.pool.key();
        ob.tick_1e6 = tick_1e6;
        ob.best_bid_band = -1;
        ob.best_ask_band = -1;
        ob.next_order_id = 1;
        ob.event_q_head = 0;
        ob.max_levels = max_levels.max(pool.n_bands as u16);
        ob.max_queue_per_level = DEFAULT_MAX_QUEUE_PER_LEVEL;

        let n = pool.n_bands as usize;
        ob.bids.clear();
        ob.asks.clear();
        for i in 0..n {
            ob.bids.push(PriceLevel { band_idx: i as i16, total_qty: 0, head: 0, tail: 0 });
            ob.asks.push(PriceLevel { band_idx: i as i16, total_qty: 0, head: 0, tail: 0 });
        }
        ob.event_q.clear();
        ob.event_q.resize(EVENT_Q_CAP, BookEvent::default());

        Ok(())
    }

    /* ----------------------- Orderbook: placement/cancel -------------------- */

    #[allow(clippy::too_many_arguments)]
    pub fn place_order(
        ctx: Context<PlaceOrder>,
        side: Side,
        qty: u64,
        limit_price_opt_1e6: Option<u64>,
        tif: TifParam,
        post_only: bool,
        reduce_only: bool,
        client_id: u64,
    ) -> Result<u64> {
        // capture immutable info first
        let pool_key = ctx.accounts.pool.key();

        let pool = &mut ctx.accounts.pool;
        let ob = &mut ctx.accounts.orderbook;
        require!(!pool.pause_orderbook && !pool.is_paused, DlmmError::Paused);
        require!(qty > 0, DlmmError::ZeroAmount);

        if Clock::get()?.slot <= pool.post_only_until_slot {
            require!(post_only, DlmmError::Paused);
        }

        let price_1e6 = match limit_price_opt_1e6 {
            Some(p) => round_to_tick(p, ob.tick_1e6),
            None => pool.last_center_price_1e6,
        };
        let target_band = map_price_to_band(pool, price_1e6)?;

        let mut remaining = qty;
        let mut order_id = 0u64;

        let stp = StpMode::from_u8(pool.stp_mode);
        let route_mode = RouteMode::from_u8(pool.route_mode);
        match route_mode {
            RouteMode::BookFirst => {
                if !post_only {
                    remaining = match_against_book(
                        ob,
                        pool,
                        pool_key,
                        side.opposite(),
                        price_1e6,
                        remaining,
                        &ctx.accounts.user.key(),
                        stp,
                    )?;
                }
                if remaining > 0 {
                    if tif.is_ioc() || post_only {
                        if post_only {
                            order_id = rest_in_book(
                                ob,
                                side,
                                target_band,
                                remaining,
                                tif,
                                reduce_only,
                                client_id,
                                ctx.accounts.user.key(),
                            )?;
                            emit!(OrderPlacedV3 {
                                event_version: EVENT_VERSION,
                                pool: pool_key,
                                order_id,
                                owner: ctx.accounts.user.key(),
                                side,
                                price_1e6,
                                qty: remaining,
                                band_idx: target_band as i16
                            });
                        }
                    } else {
                        order_id = rest_in_book(
                            ob,
                            side,
                            target_band,
                            remaining,
                            tif,
                            reduce_only,
                            client_id,
                            ctx.accounts.user.key(),
                        )?;
                        emit!(OrderPlacedV3 {
                            event_version: EVENT_VERSION,
                            pool: pool_key,
                            order_id,
                            owner: ctx.accounts.user.key(),
                            side,
                            price_1e6,
                            qty: remaining,
                            band_idx: target_band as i16
                        });
                    }
                }
            }
            RouteMode::DlmmFirst => {
                if !post_only {
                    remaining = take_from_bands(pool, pool_key, side, remaining, price_1e6)?;
                }
                if remaining > 0 {
                    if tif.is_ioc() || post_only {
                        if post_only {
                            order_id = rest_in_book(
                                ob,
                                side,
                                target_band,
                                remaining,
                                tif,
                                reduce_only,
                                client_id,
                                ctx.accounts.user.key(),
                            )?;
                            emit!(OrderPlacedV3 {
                                event_version: EVENT_VERSION,
                                pool: pool_key,
                                order_id,
                                owner: ctx.accounts.user.key(),
                                side,
                                price_1e6,
                                qty: remaining,
                                band_idx: target_band as i16
                            });
                        }
                    } else {
                        order_id = rest_in_book(
                            ob,
                            side,
                            target_band,
                            remaining,
                            tif,
                            reduce_only,
                            client_id,
                            ctx.accounts.user.key(),
                        )?;
                        emit!(OrderPlacedV3 {
                            event_version: EVENT_VERSION,
                            pool: pool_key,
                            order_id,
                            owner: ctx.accounts.user.key(),
                            side,
                            price_1e6,
                            qty: remaining,
                            band_idx: target_band as i16
                        });
                    }
                }
            }
        }

        refresh_top_of_book(ob, pool)?;
        Ok(order_id)
    }

    pub fn cancel_order(ctx: Context<MutateOrderbook>, side: Side, order_id: u64) -> Result<()> {
        let pool_key = ctx.accounts.pool.key();
        let pool = &mut ctx.accounts.pool;
        let ob = &mut ctx.accounts.orderbook;
        require!(!pool.pause_orderbook && !pool.is_paused, DlmmError::Paused);

        let removed = remove_order_linear(ob, side, order_id, Some(ctx.accounts.user.key()))?;
        require!(removed, DlmmError::NotFound);
        refresh_top_of_book(ob, pool)?;
        emit!(OrderCanceledV3 {
            event_version: EVENT_VERSION,
            pool: pool_key,
            order_id,
            owner: ctx.accounts.user.key(),
            side
        });
        Ok(())
    }

    pub fn crank_match(ctx: Context<MutateOrderbook>, max_iterations: u16) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let ob = &mut ctx.accounts.orderbook;
        require!(!pool.pause_orderbook && !pool.is_paused, DlmmError::Paused);

        let mut iters = 0u16;
        while iters < max_iterations {
            let (bb, ba) = (ob.best_bid_band, ob.best_ask_band);
            if bb < 0 || ba < 0 || (bb as i32) < (ba as i32) {
                break;
            }
            let took = cross_once(ob, pool)?;
            if !took {
                break;
            }
            iters = iters.saturating_add(1);
        }
        refresh_top_of_book(ob, pool)?;
        Ok(())
    }

    pub fn prune_expired(ctx: Context<MutateOrderbook>, max_to_prune: u16) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let ob = &mut ctx.accounts.orderbook;
        require!(!pool.pause_orderbook && !pool.is_paused, DlmmError::Paused);

        let now = Clock::get()?.slot;
        let mut pruned = 0u16;

        for lv in ob.bids.iter_mut() {
            pruned = pruned.saturating_add(prune_level(lv, now, max_to_prune - pruned));
            if pruned >= max_to_prune { break; }
        }
        if pruned < max_to_prune {
            for lv in ob.asks.iter_mut() {
                pruned = pruned.saturating_add(prune_level(lv, now, max_to_prune - pruned));
                if pruned >= max_to_prune { break; }
            }
        }
        refresh_top_of_book(ob, pool)?;
        Ok(())
    }

    pub fn view_depth(ctx: Context<ViewOrderbook>, levels: u8) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let ob = &ctx.accounts.orderbook;
        let n = pool.n_bands as usize;

        let mut out: Vec<DepthItem> = Vec::new();
        let center = pool.last_center_price_1e6;
        let mut collected = 0usize;

        let mut idxs: Vec<usize> = (0..n).collect();
        idxs.sort_by_key(|i| {
            let b = &pool.bands[*i];
            let mid = mid_price(b.lower_price_1e6, b.upper_price_1e6);
            diff_abs(center, mid)
        });

        for i in idxs {
            if collected >= levels as usize { break; }
            let bid_qty = ob.bids.get(i).map(|l| l.total_qty).unwrap_or(0);
            let ask_qty = ob.asks.get(i).map(|l| l.total_qty).unwrap_or(0);
            if bid_qty == 0 && ask_qty == 0 { continue; }
            let mid = mid_price(pool.bands[i].lower_price_1e6, pool.bands[i].upper_price_1e6);
            out.push(DepthItem { price_1e6: mid, bid_qty, ask_qty, band_idx: i as u16 });
            collected += 1;
        }

        emit!(DepthSnapshotV {
            event_version: EVENT_VERSION,
            pool: ctx.accounts.pool.key(),
            items: out
        });
        Ok(())
    }
}

/* =============================================================================
                               Core compute & helpers
============================================================================= */

fn preview_center_width(pool: &Pool) -> Result<(u64, u16)> {
    let max_y = pool.ema_y_a_bps.max(pool.ema_y_b_bps) as u32;
    let shrink_units = max_y / 1000;
    let base = pool.base_width_bps as i32;
    let shrink = (pool.width_slope_per_kbps as i32) * (shrink_units as i32);
    let width_bps = base
        .saturating_sub(shrink)
        .clamp(pool.min_width_bps as i32, pool.max_width_bps as i32) as u16;

    let diff = (pool.ema_y_a_bps as i32) - (pool.ema_y_b_bps as i32);
    let mag_kbps = (diff.abs() as u32) / 1000;
    let tilt_bps = (pool.bias_per_kbps as u32).saturating_mul(mag_kbps) as i64;
    let signed_tilt = if diff >= 0 { tilt_bps } else { -(tilt_bps) };
    let center = apply_bps_i(pool.ema_spot_1e6, signed_tilt)?;
    Ok((center, width_bps))
}

fn recompute_bands(pool: &mut Account<Pool>, enforce_cb: bool, weights_only: bool) -> Result<()> {
    let (mut center, mut width_bps) = preview_center_width(pool)?;

    if enforce_cb {
        if pool.last_center_price_1e6 > 0 {
            let prev = pool.last_center_price_1e6;
            center = center.clamp(
                apply_bps_i(prev, -(pool.max_center_move_bps as i64))?,
                apply_bps_i(prev, pool.max_center_move_bps as i64)?,
            );
        }
        if pool.last_width_bps > 0 {
            let prev = pool.last_width_bps as i32;
            let cur = width_bps as i32;
            let delta = (cur - prev).abs();
            let max = pool.max_width_change_bps as i32;
            if delta > max {
                width_bps = (if cur > prev { prev + max } else { prev - max }) as u16;
            }
        }
    }

    let n = pool.n_bands as usize;
    let mid = (n as i32 - 1) / 2;
    let decay = pool.decay_per_band_bps as i32;
    let diff = (pool.ema_y_a_bps as i32) - (pool.ema_y_b_bps as i32);
    let sign_tilt_each = if diff >= 0 { 1 } else { -1 };

    let mut new_bands = pool.bands.clone();
    new_bands.resize(n, Band::default());

    for i in 0..n {
        let idx = i as i32 - mid;
        if !weights_only {
            let delta_bps = idx * (width_bps as i32);
            let lower = apply_bps_i(center, delta_bps as i64)?;
            let upper = apply_bps_i(center, (delta_bps + width_bps as i32) as i64)?;
            require!(lower < upper, DlmmError::InvalidBandRange);
            new_bands[i].lower_price_1e6 = lower;
            new_bands[i].upper_price_1e6 = upper;
        }

        let base_weight = (10_000i32 / (n as i32))
            .saturating_sub(decay.saturating_mul(idx.abs()))
            .clamp(100, 10_000);
        let tilt_bonus =
            (pool.bias_per_kbps as i32).min(200) * if idx.signum() == sign_tilt_each { 1 } else { 0 };
        new_bands[i].weight_bps = (base_weight + tilt_bonus).clamp(100, 10_000) as u16;

        if i >= pool.bands.len() {
            new_bands[i].is_active = true;
        } else {
            let old = &pool.bands[i];
            new_bands[i].reserves_a = old.reserves_a;
            new_bands[i].reserves_b = old.reserves_b;
            new_bands[i].total_shares = old.total_shares;
            new_bands[i].fee_growth_a_1e18 = old.fee_growth_a_1e18;
            new_bands[i].fee_growth_b_1e18 = old.fee_growth_b_1e18;
            new_bands[i].util_a = old.util_a;
            new_bands[i].util_b = old.util_b;
            new_bands[i].is_active = old.is_active;
        }
    }

    for i in 1..n {
        if !weights_only {
            require!(
                new_bands[i - 1].upper_price_1e6 <= new_bands[i].lower_price_1e6,
                DlmmError::NonMonotonicBands
            );
        }
    }

    // normalize
    let total_raw: u128 = new_bands
        .iter()
        .map(|b| b.weight_bps as u128)
        .sum::<u128>()
        .max(1);
    let mut sum: u32 = 0;
    for (i, b) in new_bands.iter_mut().enumerate() {
        let w_prop = (u128::from(b.weight_bps) * 10_000u128 / total_raw) as i64;
        let w_cb = if enforce_cb && i < pool.bands.len() {
            let prev = pool.bands[i].weight_bps as i64;
            w_prop.clamp(
                prev - pool.max_weight_shift_bps as i64,
                prev + pool.max_weight_shift_bps as i64,
            )
        } else {
            w_prop
        }
        .clamp(1, 10_000);
        b.weight_bps = w_cb as u16;
        sum = sum.saturating_add(b.weight_bps as u32);
    }
    if let Some(last) = new_bands.last_mut() {
        if sum != 10_000 {
            let adj = if sum > 10_000 { sum - 10_000 } else { 10_000 - sum } as i32;
            let nv = (last.weight_bps as i32 + if sum > 10_000 { -adj } else { adj })
                .clamp(1, 10_000);
            last.weight_bps = nv as u16;
        }
    }

    pool.bands = new_bands;
    pool.last_width_bps = width_bps;
    pool.last_center_price_1e6 = center;
    pool.total_weight_bps = pool.bands.iter().map(|b| b.weight_bps as u32).sum::<u32>();
    Ok(())
}

/* =============================================================================
                                  Orderbook helpers & other helpers
============================================================================= */

fn round_to_tick(p: u64, tick: u64) -> u64 {
    if tick == 0 { return p; }
    (p / tick) * tick
}
fn map_price_to_band(pool: &Pool, price: u64) -> Result<u16> {
    let n = pool.n_bands as usize;
    let mut best_i = 0usize;
    let mut best_d = u64::MAX;
    for i in 0..n {
        let b = &pool.bands[i];
        let mid = mid_price(b.lower_price_1e6, b.upper_price_1e6);
        let d = diff_abs(price, mid);
        if d < best_d {
            best_d = d;
            best_i = i;
        }
    }
    Ok(best_i as u16)
}
fn mid_price(a: u64, b: u64) -> u64 {
    (a / 2).saturating_add(b / 2)
}
fn diff_abs(a: u64, b: u64) -> u64 {
    if a >= b { a - b } else { b - a }
}

fn refresh_top_of_book(ob: &OrderBook, pool: &mut Pool) -> Result<()> {
    let mut bb: Option<u64> = None;
    for (i, l) in ob.bids.iter().enumerate().rev() {
        if l.total_qty > 0 {
            let b = &pool.bands[i];
            bb = Some(mid_price(b.lower_price_1e6, b.upper_price_1e6));
            break;
        }
    }
    let mut ba: Option<u64> = None;
    for (i, l) in ob.asks.iter().enumerate() {
        if l.total_qty > 0 {
            let b = &pool.bands[i];
            ba = Some(mid_price(b.lower_price_1e6, b.upper_price_1e6));
            break;
        }
    }
    pool.best_bid_1e6 = bb.unwrap_or(0);
    pool.best_ask_1e6 = ba.unwrap_or(u64::MAX);
    Ok(())
}

fn rest_in_book(
    ob: &mut OrderBook,
    side: Side,
    band_idx: u16,
    qty: u64,
    tif: TifParam,
    reduce_only: bool,
    client_id: u64,
    owner: Pubkey,
) -> Result<u64> {
    let level = match side {
        Side::Bid => ob.bids.get_mut(band_idx as usize).ok_or(DlmmError::InvalidBandIndex)?,
        Side::Ask => ob.asks.get_mut(band_idx as usize).ok_or(DlmmError::InvalidBandIndex)?,
    };
    let q_len = (level.tail - level.head) as usize;
    require!(q_len < ob.max_queue_per_level as usize, DlmmError::ParamOutOfRange);

    let id = ob.next_order_id;
    ob.next_order_id = ob.next_order_id.saturating_add(1);
    let expiry = tif.as_expiry(Clock::get()?.slot);

    level.total_qty = level.total_qty.saturating_add(qty);
    level.tail = level.tail.saturating_add(1);

    push_event(
        ob,
        BookEvent::Place {
            order_id: id,
            side,
            band_idx: band_idx as i16,
            owner,
            qty,
            client_id,
            tif_expiry: expiry,
            reduce_only,
        },
    );
    Ok(id)
}

fn remove_order_linear(ob: &mut OrderBook, side: Side, order_id: u64, owner_opt: Option<Pubkey>) -> Result<bool> {
    for i in (0..ob.event_q.len()).rev() {
        match ob.event_q[i] {
            BookEvent::Place {
                order_id: oid,
                side: s,
                band_idx,
                owner,
                qty,
                ..
            } if oid == order_id && s == side && owner_opt.map(|o| o == owner).unwrap_or(true) => {
                let lvl = match side {
                    Side::Bid => ob.bids.get_mut(band_idx as usize),
                    Side::Ask => ob.asks.get_mut(band_idx as usize),
                };
                if let Some(level) = lvl {
                    if level.total_qty >= qty {
                        level.total_qty -= qty;
                    } else {
                        level.total_qty = 0;
                    }
                    push_event(
                        ob,
                        BookEvent::Out {
                            order_id,
                            reason: OUT_REASON_CANCEL,
                        },
                    );
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn cross_once(ob: &mut OrderBook, pool: &Pool) -> Result<bool> {
    let mut bid_i: Option<usize> = None;
    for (i, l) in ob.bids.iter().enumerate().rev() {
        if l.total_qty > 0 {
            bid_i = Some(i);
            break;
        }
    }
    let mut ask_i: Option<usize> = None;
    for (i, l) in ob.asks.iter().enumerate() {
        if l.total_qty > 0 {
            ask_i = Some(i);
            break;
        }
    }
    let (bi, ai) = match (bid_i, ask_i) {
        (Some(bi), Some(ai)) if bi >= ai => (bi, ai),
        _ => return Ok(false),
    };
    let price_band = ai;
    let band = &pool.bands[price_band];
    let px = mid_price(band.lower_price_1e6, band.upper_price_1e6);

    let lot = 1u64;
    let b_lvl = &mut ob.bids[bi];
    let a_lvl = &mut ob.asks[ai];
    if b_lvl.total_qty == 0 || a_lvl.total_qty == 0 {
        return Ok(false);
    }
    b_lvl.total_qty = b_lvl.total_qty.saturating_sub(lot);
    a_lvl.total_qty = a_lvl.total_qty.saturating_sub(lot);

    push_event(ob, BookEvent::Fill { order_id: 0, qty: lot, price_1e6: px, side: Side::Bid });
    push_event(ob, BookEvent::Fill { order_id: 0, qty: lot, price_1e6: px, side: Side::Ask });

    Ok(true)
}

fn match_against_book(
    ob: &mut OrderBook,
    pool: &Pool,
    pool_pk: Pubkey,
    hit_side: Side,
    limit_price_1e6: u64,
    mut qty: u64,
    taker: &Pubkey,
    _stp: StpMode,
) -> Result<u64> {
    let n = pool.n_bands as usize;
    let mut idxs: Vec<usize> = Vec::new();

    match hit_side {
        Side::Ask => {
            for i in 0..n {
                let mid = mid_price(pool.bands[i].lower_price_1e6, pool.bands[i].upper_price_1e6);
                if mid <= limit_price_1e6 {
                    idxs.push(i);
                } else {
                    break;
                }
            }
        }
        Side::Bid => {
            for i in (0..n).rev() {
                let mid = mid_price(pool.bands[i].lower_price_1e6, pool.bands[i].upper_price_1e6);
                if mid >= limit_price_1e6 {
                    idxs.push(i);
                } else {
                    break;
                }
            }
        }
    }

    for i in idxs {
        if qty == 0 { break; }
        let level = match hit_side {
            Side::Ask => &mut ob.asks[i],
            Side::Bid => &mut ob.bids[i],
        };
        if level.total_qty == 0 { continue; }

        let take = qty.min(level.total_qty);
        level.total_qty -= take;

        let price = mid_price(pool.bands[i].lower_price_1e6, pool.bands[i].upper_price_1e6);
        let taker_bps = pool.fee_current_bps.max(pool.taker_min_bps);
        let maker_rebate_bps = pool.maker_rebate_max_bps.min(taker_bps);

        emit!(OrderFilledV3 {
            event_version: EVENT_VERSION,
            pool: pool_pk,
            taker: *taker,
            side: hit_side.opposite(),
            qty: take,
            price_1e6: price,
            taker_fee_bps: taker_bps,
            maker_rebate_bps
        });

        qty -= take;
    }

    Ok(qty)
}

/* Fixed borrow: copy fee bps before mutable borrow of pool.bands[i] */
fn take_from_bands(pool: &mut Account<Pool>, pool_pk: Pubkey, side: Side, mut qty: u64, limit_price_1e6: u64) -> Result<u64> {
    let n = pool.n_bands as usize;
    let mut idxs: Vec<usize> = (0..n).collect();
    let center = pool.last_center_price_1e6;
    idxs.sort_by_key(|i| {
        let b = &pool.bands[*i];
        let mid = mid_price(b.lower_price_1e6, b.upper_price_1e6);
        diff_abs(center, mid)
    });

    // read fee/current bps into locals BEFORE taking mutable borrows into bands
    let fee_current_bps_u128: u128 = pool.fee_current_bps as u128;
    let fee_current_bps_u16: u16 = pool.fee_current_bps;

    for i in idxs {
        if qty == 0 { break; }
        let b = &mut pool.bands[i];
        if !b.is_active { continue; }
        let mid = mid_price(b.lower_price_1e6, b.upper_price_1e6);
        match side {
            Side::Bid => { if mid > limit_price_1e6 { continue; } }
            Side::Ask => { if mid < limit_price_1e6 { continue; } }
        }

        let cap = match side {
            Side::Bid => b.reserves_b,
            Side::Ask => b.reserves_a,
        };
        if cap == 0 { continue; }

        let trade = qty.min(cap);
        match side {
            Side::Bid => { b.reserves_b = b.reserves_b.saturating_sub(trade); b.reserves_a = b.reserves_a.saturating_add(trade); }
            Side::Ask => { b.reserves_a = b.reserves_a.saturating_sub(trade); b.reserves_b = b.reserves_b.saturating_add(trade); }
        }

        let fee_bps = fee_current_bps_u128;
        let growth = (u128::from(trade) * fee_bps) * 1_000_000_000_000_000u128 / 10_000u128;
        b.fee_growth_a_1e18 = b.fee_growth_a_1e18.saturating_add(growth);
        b.fee_growth_b_1e18 = b.fee_growth_b_1e18.saturating_add(growth);

        emit!(SwapFilledV {
            event_version: EVENT_VERSION,
            pool: pool_pk,
            side,
            qty: trade,
            price_1e6: mid,
            band_idx: i as u16,
            fee_bps: fee_current_bps_u16
        });

        qty -= trade;
    }
    Ok(qty)
}

fn prune_level(level: &mut PriceLevel, _now_slot: u64, mut left: u16) -> u16 {
    if left == 0 { return 0; }
    let mut pruned = 0u16;
    while left > 0 && level.head < level.tail && level.total_qty == 0 {
        level.head = level.head.saturating_add(1);
        pruned = pruned.saturating_add(1);
        left = left.saturating_sub(1);
    }
    pruned
}

fn push_event(ob: &mut OrderBook, ev: BookEvent) {
    let idx = (ob.event_q_head as usize) % ob.event_q.len();
    ob.event_q[idx] = ev;
    ob.event_q_head = ob.event_q_head.wrapping_add(1);
}

/* =============================================================================
                                   Helpers
============================================================================= */

fn apply_bps_i(value: u64, bps: i64) -> Result<u64> {
    let scale: i128 = 10_000;
    let v = value as i128;
    let adj = (scale + (bps as i128)).max(0);
    let res = v.checked_mul(adj).ok_or(DlmmError::MathOverflow)? / scale;
    Ok(res.clamp(0, u64::MAX as i128) as u64)
}
fn ema_step_u16(prev: u16, newv: u16, alpha_bps: u16) -> Result<u16> {
    let prev = prev as i64;
    let newv = newv as i64;
    let a = alpha_bps as i64;
    let out = prev + (a * (newv - prev)) / 10_000;
    Ok(out.clamp(0, u16::MAX as i64) as u16)
}
fn ema_step_u64(prev: u64, newv: u64, alpha_bps: u16) -> Result<u64> {
    let prev = prev as i128;
    let newv = newv as i128;
    let a = alpha_bps as i128;
    let out = prev + (a * (newv - prev)) / 10_000;
    Ok(out.clamp(0, u64::MAX as i128) as u64)
}
fn diff_bps_u64(a: u64, b: u64) -> Result<u64> {
    let a = a.max(1) as i128;
    let b = b as i128;
    let num = (b - a)
        .abs()
        .checked_mul(10_000)
        .ok_or(DlmmError::MathOverflow)?;
    Ok((num / a) as u64)
}
fn diff_i_bps(a: i32, b: i32) -> Result<i32> {
    Ok((b - a).abs())
}
fn digest_bands(bands: &Vec<Band>) -> [u8; 32] {
    let mut bytes: Vec<u8> = Vec::with_capacity(bands.len() * 18);
    for b in bands {
        bytes.extend_from_slice(&b.lower_price_1e6.to_le_bytes());
        bytes.extend_from_slice(&b.upper_price_1e6.to_le_bytes());
        bytes.extend_from_slice(&b.weight_bps.to_le_bytes());
    }
    keccak::hash(&bytes).0
}
fn passes_ratio_guard(pool: &Pool, a: u64, b: u64) -> Result<bool> {
    if a == 0 || b == 0 {
        return Ok(true);
    }
    let price = pool.last_center_price_1e6.max(1);
    let r = (u128::from(a) * u128::from(price) / u128::from(b)) as u64;
    let r_bps = (r / 100) as u64;
    Ok(
        r_bps >= pool.deposit_ratio_min_bps as u64 && r_bps <= pool.deposit_ratio_max_bps as u64,
    )
}
fn mark_inactive_by_floor(pool: &mut Pool) {
    for b in pool.bands.iter_mut() {
        b.is_active = !(b.reserves_a < pool.inactive_floor_a && b.reserves_b < pool.inactive_floor_b);
    }
}
fn compact_active_bands(pool: &mut Pool) {
    let mut out = Vec::with_capacity(pool.bands.len());
    for b in pool.bands.iter() {
        if b.is_active {
            out.push(b.clone());
        }
    }
    if out.is_empty() {
        out = pool.bands.clone();
    }
    pool.n_bands = out.len() as u8;
    pool.bands = out;
}
fn renormalize_active_weights(pool: &mut Pool) -> Result<()> {
    let sum_active: u128 = pool
        .bands
        .iter()
        .filter(|b| b.is_active)
        .map(|b| b.weight_bps as u128)
        .sum::<u128>()
        .max(1);
    let mut sum: u32 = 0;
    for b in pool.bands.iter_mut() {
        if b.is_active {
            b.weight_bps = (u128::from(b.weight_bps) * 10_000u128 / sum_active) as u16;
            sum = sum.saturating_add(b.weight_bps as u32);
        } else {
            b.weight_bps = 0;
        }
    }
    if let Some(last) = pool.bands.iter_mut().rev().find(|b| b.is_active) {
        if sum != 10_000 {
            let adj = if sum > 10_000 { sum - 10_000 } else { 10_000 - sum } as i32;
            last.weight_bps =
                (last.weight_bps as i32 + if sum > 10_000 { -adj } else { adj }).clamp(1, 10_000) as u16;
        }
    }
    pool.total_weight_bps = pool.bands.iter().map(|b| b.weight_bps as u32).sum::<u32>();
    Ok(())
}
fn mul_div_1e18(a: u64, growth: u128) -> u64 {
    let num = (u128::from(a)).saturating_mul(growth);
    (num / 1_000_000_000_000_000_000u128) as u64
}

/* =============================================================================
   pay_bounty_if_any: fixed lifetimes by making this generic over 'info so that
   Program<'info, Token> and Account<'info, Pool> share the same lifetime.
============================================================================= */

fn pay_bounty_if_any<'info>(
    token_program: &Program<'info, Token>,
    pool_mut: &mut Account<'info, Pool>,
    treasury_a: &Account<'info, TokenAccount>,
    treasury_b: &Account<'info, TokenAccount>,
    dst_a: &Account<'info, TokenAccount>,
    dst_b: &Account<'info, TokenAccount>,
    d_center_bps: u32,
    d_width_bps: u32,
) -> Result<()> {
    // reborrow immutable view for checks
    let pool_ai: &Pool = &*pool_mut;
    if pool_ai.is_paused || pool_ai.g_pending.is_some() {
        return Ok(());
    }

    let mut change = d_center_bps.saturating_add(d_width_bps) as u128;
    if pool_ai.needs_update {
        change = change.saturating_mul((10_000u128 + pool_ai.bounty_boost_bps as u128)) / 10_000u128;
    }

    let raw = change.saturating_mul(pool_ai.bounty_rate_microunits as u128) / 1_000_000u128;
    let amount = raw.min(pool_ai.bounty_max as u128) as u64;
    if amount == 0 {
        return Ok(());
    }

    // create seeds from the mutable pool state (pool_mut)
    let seeds = pool_signer_seeds(&*pool_mut);
    let signer = &[&seeds[..]];

    if treasury_a.amount >= amount && dst_a.mint == treasury_a.mint {
        token::transfer(
            CpiContext::new_with_signer(
                token_program.to_account_info(),
                Transfer {
                    from: treasury_a.to_account_info(),
                    to: dst_a.to_account_info(),
                    authority: pool_mut.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;
    } else if treasury_b.amount >= amount && dst_b.mint == treasury_b.mint {
        token::transfer(
            CpiContext::new_with_signer(
                token_program.to_account_info(),
                Transfer {
                    from: treasury_b.to_account_info(),
                    to: dst_b.to_account_info(),
                    authority: pool_mut.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;
    }
    Ok(())
}

fn assert_invariants(
    mint_a: &Account<Mint>,
    mint_b: &Account<Mint>,
    vault_a: &Account<TokenAccount>,
    vault_b: &Account<TokenAccount>,
    pool: &Pool,
) -> Result<()> {
    require!(
        vault_a.mint == mint_a.key() && vault_b.mint == mint_b.key(),
        DlmmError::VaultMintMismatch
    );
    require!(
        (pool.n_bands as usize) == pool.bands.len(),
        DlmmError::InvariantViolated
    );
    let mut last = 0u64;
    let mut sum = 0u32;
    for (i, b) in pool.bands.iter().enumerate() {
        if i > 0 {
            require!(last <= b.lower_price_1e6, DlmmError::NonMonotonicBands);
        }
        require!(b.lower_price_1e6 < b.upper_price_1e6, DlmmError::InvalidBandRange);
        last = b.upper_price_1e6;
        sum = sum.saturating_add(b.weight_bps as u32);
    }
    require!(sum == 10_000, DlmmError::WeightSumInvalid);
    require!(
        pool.last_width_bps >= pool.min_width_bps && pool.last_width_bps <= pool.max_width_bps,
        DlmmError::ParamOutOfRange
    );
    Ok(())
}

fn pool_signer_seeds<'a>(pool: &'a Pool) -> [&'a [u8]; 5] {
    let bump_slice: &'a [u8] = core::slice::from_ref(&pool.bump);
    [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref(), bump_slice]
}

/* =============================================================================
                                   Accounts & Storage
============================================================================= */

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct GovProposal {
    pub new: SettableParamsV3,
    pub queued_at: u64,
    pub earliest_exec: u64,
    pub deadline: u64,
    pub executed: bool,
}

#[derive(Accounts)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,

    #[account(
        init,
        payer = payer,
        space = POOL_SPACE,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init,
        payer = payer,
        token::mint = mint_a,
        token::authority = pool,
        seeds = [b"v3".as_ref(), b"vault".as_ref(), pool.key().as_ref(), mint_a.key().as_ref()],
        bump
    )]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(
        init,
        payer = payer,
        token::mint = mint_b,
        token::authority = pool,
        seeds = [b"v3".as_ref(), b"vault".as_ref(), pool.key().as_ref(), mint_b.key().as_ref()],
        bump
    )]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = payer,
        token::mint = mint_a,
        token::authority = pool,
        seeds = [b"v3".as_ref(), b"treasury".as_ref(), pool.key().as_ref(), mint_a.key().as_ref()],
        bump
    )]
    pub treasury_a: Account<'info, TokenAccount>,
    #[account(
        init,
        payer = payer,
        token::mint = mint_b,
        token::authority = pool,
        seeds = [b"v3".as_ref(), b"treasury".as_ref(), pool.key().as_ref(), mint_b.key().as_ref()],
        bump
    )]
    pub treasury_b: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PostYieldsAndUpdate<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    pub oracle_signer_opt: Option<UncheckedAccount<'info>>,

    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, address = pool.treasury_a)]
    pub treasury_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.treasury_b)]
    pub treasury_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub caller_ata_a: Account<'info, TokenAccount>,
    #[account(mut)]
    pub caller_ata_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub mint_a: Account<'info, Mint>,
    #[account(mut)]
    pub mint_b: Account<'info, Mint>,
    #[account(mut, address = pool.vault_a)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault_b)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub metrics: Option<Account<'info, MetricsRing>>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ViewPool<'info> {
    #[account(
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
#[instruction(band_idx: u8, amount_a: u64, amount_b: u64, receipt_nonce: u64, min_unlock_after_slots: u64)]
pub struct AddLiquidity<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, address = pool.vault_a)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault_b)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_ata_a.mint == pool.mint_a)]
    pub user_ata_a: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_ata_b.mint == pool.mint_b)]
    pub user_ata_b: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + Position::SIZE,
        seeds = [b"v3".as_ref(), b"pos".as_ref(), pool.key().as_ref(), user.key().as_ref(), receipt_nonce.to_le_bytes().as_ref()],
        bump
    )]
    pub position: Account<'info, Position>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,
}

#[derive(Accounts)]
pub struct RemoveLiquidity<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, address = pool.vault_a)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault_b)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_ata_a.mint == pool.mint_a)]
    pub user_ata_a: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_ata_b.mint == pool.mint_b)]
    pub user_ata_b: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pos".as_ref(), pool.key().as_ref(), position.owner.as_ref(), position.receipt_nonce.to_le_bytes().as_ref()],
        bump = position.bump,
        has_one = pool @ DlmmError::Unauthorized,
        close = user
    )]
    pub position: Account<'info, Position>,

    pub token_program: Program<'info, Token>,
    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,
}

#[derive(Accounts)]
pub struct WithPosition<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pos".as_ref(), pool.key().as_ref(), position.owner.as_ref(), position.receipt_nonce.to_le_bytes().as_ref()],
        bump = position.bump,
        has_one = pool @ DlmmError::Unauthorized
    )]
    pub position: Account<'info, Position>,

    #[account(mut, address = pool.treasury_a)]
    pub treasury_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.treasury_b)]
    pub treasury_b: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_ata_a: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_ata_b: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RiskScoped<'info> {
    #[account(mut)]
    pub risk_admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump,
        constraint = pool.risk_admin == risk_admin.key() || is_admin(&pool.admins, &risk_admin.key())
    )]
    pub pool: Account<'info, Pool>,
    #[account(mut, address = pool.vault_a)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault_b)]
    pub vault_b: Account<'info, TokenAccount>,
    #[account(mut, address = pool.treasury_a)]
    pub treasury_a: Account<'info, TokenAccount>,
    #[account(mut, address = pool.treasury_b)]
    pub treasury_b: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct OpsScoped<'info> {
    pub ops_admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump,
        constraint = pool.ops_admin == ops_admin.key() || is_admin(&pool.admins, &ops_admin.key())
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct FeeScoped<'info> {
    pub fee_admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump,
        constraint = pool.fee_admin == fee_admin.key() || is_admin(&pool.admins, &fee_admin.key())
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct AdminScoped<'info> {
    pub any_admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump,
        constraint = is_admin(&pool.admins, &any_admin.key())
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct AdminMultisig<'info> {
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct InitMetrics<'info> {
    #[account(
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        init,
        payer = payer,
        space = METRICS_SPACE,
        seeds = [b"v3".as_ref(), b"metrics".as_ref(), pool.key().as_ref()],
        bump
    )]
    pub metrics: Account<'info, MetricsRing>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

/* --------- Orderbook account contexts --------- */

#[derive(Accounts)]
pub struct InitOrderBook<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        init,
        payer = payer,
        space = ORDERBOOK_SPACE,
        seeds = [b"v3".as_ref(), b"orderbook".as_ref(), pool.key().as_ref()],
        bump
    )]
    pub orderbook: Account<'info, OrderBook>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PlaceOrder<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"orderbook".as_ref(), pool.key().as_ref()],
        bump = orderbook.bump,
        has_one = pool @ DlmmError::Unauthorized
    )]
    pub orderbook: Account<'info, OrderBook>,
}

#[derive(Accounts)]
pub struct MutateOrderbook<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        seeds = [b"v3".as_ref(), b"orderbook".as_ref(), pool.key().as_ref()],
        bump = orderbook.bump,
        has_one = pool @ DlmmError::Unauthorized
    )]
    pub orderbook: Account<'info, OrderBook>,
}

#[derive(Accounts)]
pub struct ViewOrderbook<'info> {
    #[account(
        seeds = [b"v3".as_ref(), b"pool".as_ref(), pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        seeds = [b"v3".as_ref(), b"orderbook".as_ref(), pool.key().as_ref()],
        bump = orderbook.bump,
        has_one = pool @ DlmmError::Unauthorized
    )]
    pub orderbook: Account<'info, OrderBook>,
}

/* =============================================================================
                                   Storage
============================================================================= */

#[account]
pub struct Pool {
    pub version: u8,
    pub bump: u8,

    // multisig
    pub admin_threshold: u8,
    pub admins: [Pubkey; MAX_ADMINS],

    // scoped roles
    pub risk_admin: Pubkey,
    pub ops_admin: Pubkey,
    pub fee_admin: Pubkey,

    // assets
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub treasury_a: Pubkey,
    pub treasury_b: Pubkey,

    // updater/oracle
    pub updater: Pubkey,
    pub oracle_signer: Option<Pubkey>,

    // band params
    pub base_width_bps: u16,
    pub min_width_bps: u16,
    pub max_width_bps: u16,
    pub width_slope_per_kbps: u16,
    pub bias_per_kbps: u16,
    pub decay_per_band_bps: u16,
    pub n_bands: u8,

    // EMA/TWAP/vol
    pub y_a_bps: u16,
    pub y_b_bps: u16,
    pub spot_price_1e6: u64,
    pub ema_y_a_bps: u16,
    pub ema_y_b_bps: u16,
    pub ema_spot_1e6: u64,
    pub alpha_y_bps: u16,
    pub alpha_spot_bps: u16,
    pub alpha_twap_bps: u16,
    pub alpha_vol_bps: u16,
    pub twap_center_1e6: u64,
    pub max_twap_dev_bps: u16,
    pub vol_ema_bps: u16,

    // dynamic fee
    pub fee_base_bps: u16,
    pub fee_k_per_bps: u16,
    pub fee_max_bps: u16,
    pub fee_current_bps: u16,
    pub maker_rebate_max_bps: u16,
    pub taker_min_bps: u16,

    // CBs & cooldown
    pub max_center_move_bps: u16,
    pub max_width_change_bps: u16,
    pub max_weight_shift_bps: u16,
    pub min_update_interval_slots: u32,
    pub last_update_slot: u64,

    // hysteresis
    pub hyst_center_bps: u16,
    pub hyst_width_bps: u16,
    pub hyst_required_n: u8,
    pub hyst_ctr_center: u8,
    pub hyst_ctr_width: u8,

    // deposit ratio guard
    pub deposit_ratio_min_bps: u16,
    pub deposit_ratio_max_bps: u16,

    // inactive floors
    pub inactive_floor_a: u64,
    pub inactive_floor_b: u64,

    // bounty/incentives
    pub bounty_rate_microunits: u64,
    pub bounty_max: u64,
    pub stale_slots_for_boost: u64,
    pub bounty_boost_bps: u16,
    pub needs_update: bool,
    pub min_cu_price: u64,

    // diagnostics
    pub last_width_bps: u16,
    pub last_center_price_1e6: u64,
    pub total_weight_bps: u32,

    // flags
    pub is_paused: bool,
    pub pause_bands: bool,
    pub pause_deposits: bool,
    pub pause_withdraws: bool,
    pub pause_orderbook: bool,
    pub post_only_until_slot: u64,

    // governance pending (timelock)
    pub g_pending: Option<GovProposal>,

    // mint rotation proposals
    pub proposed_mint_a: Option<Pubkey>,
    pub proposed_mint_b: Option<Pubkey>,

    // routing / stp / book cache
    pub stp_mode: u8,
    pub route_mode: u8,
    pub best_bid_1e6: u64,
    pub best_ask_1e6: u64,
    pub book_depth_bps: u16,

    // bands
    pub bands: Vec<Band>,

    pub _reserved: [u8; 128],
}


#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default)]
pub struct Band {
    pub lower_price_1e6: u64,
    pub upper_price_1e6: u64,
    pub weight_bps: u16,
    pub fee_growth_a_1e18: u128,
    pub fee_growth_b_1e18: u128,
    pub reserves_a: u64,
    pub reserves_b: u64,
    pub total_shares: u64,
    pub util_a: u64,
    pub util_b: u64,
    pub is_active: bool,
}

#[account]
pub struct Position {
    pub bump: u8,
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub band_idx: u8,
    pub shares: u64,
    pub last_fee_growth_a_1e18: u128,
    pub last_fee_growth_b_1e18: u128,
    pub receipt_nonce: u64,
    pub min_unlock_slot: u64,
    pub approved: Option<Pubkey>,
}
impl Position {
    // size estimate (adjust if you change fields)
    pub const SIZE: usize = 1 + 32 + 32 + 1 + 8 + 16 + 16 + 8 + 8 + 1 + 32;
}

/* --------------------------- Metrics & BandBook & OrderBook --------------- */

#[account]
pub struct MetricsRing {
    pub bump: u8,
    pub pool: Pubkey,
    pub capacity: u16,
    pub head: u16,
    pub count: u16,
    pub items: Vec<MetricItem>,
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct MetricItem {
    pub slot: u64,
    pub center_price_1e6: u64,
    pub width_bps: u16,
    pub total_weight_bps: u32,
    pub hash: [u8; 32],
}
fn push_metric(
    m: &mut MetricsRing,
    slot: u64,
    center: u64,
    width: u16,
    total_w: u32,
    hash: [u8; 32],
) {
    let cap = METRICS_CAP as usize;
    if m.items.len() < cap {
        m.items.resize(cap, MetricItem::default());
    }
    let idx = (m.head as usize) % cap;
    m.items[idx] = MetricItem {
        slot,
        center_price_1e6: center,
        width_bps: width,
        total_weight_bps: total_w,
        hash,
    };
    m.head = m.head.wrapping_add(1);
    m.count = m.count.saturating_add(1).min(m.capacity);
}

#[account]
pub struct BandBook {
    pub bump: u8,
    pub pool: Pubkey,
    pub page: u16,
    pub entries: Vec<BandCompact>,
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default)]
pub struct BandCompact {
    pub lower_price_1e6: u64,
    pub upper_price_1e6: u64,
    pub weight_bps: u16,
}

#[account]
pub struct OrderBook {
    pub bump: u8,
    pub pool: Pubkey,
    pub tick_1e6: u64,
    pub best_bid_band: i16,
    pub best_ask_band: i16,
    pub next_order_id: u64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub event_q_head: u16,
    pub event_q: Vec<BookEvent>,
    pub max_levels: u16,
    pub max_queue_per_level: u16,
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default)]
pub struct PriceLevel {
    pub band_idx: i16,
    pub total_qty: u64,
    pub head: u32,
    pub tail: u32,
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub enum BookEvent {
    Fill { order_id: u64, qty: u64, price_1e6: u64, side: Side },
    Out { order_id: u64, reason: u8 },
    Place {
        order_id: u64,
        side: Side,
        band_idx: i16,
        owner: Pubkey,
        qty: u64,
        client_id: u64,
        tif_expiry: u64,
        reduce_only: bool,
    },
}
impl Default for BookEvent {
    fn default() -> Self {
        BookEvent::Out { order_id: 0, reason: 0 }
    }
}

/* ------------------------------ Params & Enums & Events ------------------- */

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitParamsV3 {
    pub admins: [Pubkey; MAX_ADMINS],
    pub admin_threshold: u8,
    pub risk_admin: Pubkey,
    pub ops_admin: Pubkey,
    pub fee_admin: Pubkey,

    pub updater: Pubkey,
    pub oracle_signer: Option<Pubkey>,

    pub n_bands: u8,
    pub base_width_bps: u16,
    pub min_width_bps: u16,
    pub max_width_bps: u16,
    pub width_slope_per_kbps: u16,
    pub bias_per_kbps: u16,
    pub decay_per_band_bps: u16,

    pub alpha_y_bps: u16,
    pub alpha_spot_bps: u16,
    pub alpha_twap_bps: u16,
    pub alpha_vol_bps: u16,
    pub max_twap_dev_bps: u16,

    pub fee_base_bps: u16,
    pub fee_k_per_bps: u16,
    pub fee_max_bps: u16,

    pub initial_y_a_bps: u16,
    pub initial_y_b_bps: u16,
    pub initial_spot_price_1e6: u64,

    pub hyst_center_bps: u16,
    pub hyst_width_bps: u16,
    pub hyst_required_n: u8,

    pub deposit_ratio_min_bps: u16,
    pub deposit_ratio_max_bps: u16,

    pub inactive_floor_a: u64,
    pub inactive_floor_b: u64,

    pub bounty_rate_microunits: u64,
    pub bounty_max: u64,
    pub stale_slots_for_boost: u64,
    pub bounty_boost_bps: u16,
    pub min_cu_price: u64,

    pub max_center_move_bps: u16,
    pub max_width_change_bps: u16,
    pub max_weight_shift_bps: u16,
    pub min_update_interval_slots: u32,

    pub maker_rebate_max_bps: u16,
    pub taker_min_bps: u16,
    pub stp_mode: StpMode,
    pub route_mode: RouteMode,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct SettableParamsV3 {
    pub n_bands: Option<u8>,
    pub base_width_bps: Option<u16>,
    pub min_width_bps: Option<u16>,
    pub max_width_bps: Option<u16>,
    pub width_slope_per_kbps: Option<u16>,
    pub bias_per_kbps: Option<u16>,
    pub decay_per_band_bps: Option<u16>,

    pub alpha_y_bps: Option<u16>,
    pub alpha_spot_bps: Option<u16>,
    pub alpha_twap_bps: Option<u16>,
    pub alpha_vol_bps: Option<u16>,
    pub max_twap_dev_bps: Option<u16>,

    pub fee_base_bps: Option<u16>,
    pub fee_k_per_bps: Option<u16>,
    pub fee_max_bps: Option<u16>,

    pub hyst_center_bps: Option<u16>,
    pub hyst_width_bps: Option<u16>,
    pub hyst_required_n: Option<u8>,

    pub deposit_ratio_min_bps: Option<u16>,
    pub deposit_ratio_max_bps: Option<u16>,

    pub inactive_floor_a: Option<u64>,
    pub inactive_floor_b: Option<u64>,

    pub bounty_rate_microunits: Option<u64>,
    pub bounty_max: Option<u64>,
    pub stale_slots_for_boost: Option<u64>,
    pub bounty_boost_bps: Option<u16>,
    pub min_cu_price: Option<u64>,

    pub max_center_move_bps: Option<u16>,
    pub max_width_change_bps: Option<u16>,
    pub max_weight_shift_bps: Option<u16>,
    pub min_update_interval_slots: Option<u32>,

    pub maker_rebate_max_bps: Option<u16>,
    pub taker_min_bps: Option<u16>,
    pub stp_mode: Option<StpMode>,
    pub route_mode: Option<RouteMode>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct PauseFlags {
    pub is_paused: Option<bool>,
    pub pause_bands: Option<bool>,
    pub pause_deposits: Option<bool>,
    pub pause_withdraws: Option<bool>,
    pub pause_orderbook: Option<bool>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum Side { Bid, Ask }
impl Side {
    pub fn opposite(self) -> Side { match self { Side::Bid => Side::Ask, Side::Ask => Side::Bid } }
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub enum RouteMode { BookFirst, DlmmFirst }
impl RouteMode {
    pub fn from_u8(v: u8) -> RouteMode { if v == 0 { RouteMode::BookFirst } else { RouteMode::DlmmFirst } }
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum StpMode { None, DecrementAndCancel, CancelNewest, CancelOldest }
impl StpMode {
    pub fn from_u8(v: u8) -> StpMode {
        match v { 1 => StpMode::DecrementAndCancel, 2 => StpMode::CancelNewest, 3 => StpMode::CancelOldest, _ => StpMode::None }
    }
}
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct TifParam { pub kind: u8, pub gtt_expiry_slot: u64 }
impl TifParam {
    pub fn is_ioc(&self) -> bool { self.kind == 0 }
    pub fn as_expiry(&self, now: u64) -> u64 { match self.kind { 0 => now, 1 => u64::MAX, _ => self.gtt_expiry_slot } }
}

/* ------------------------------ Events ------------------------------------ */

pub const EVENT_VERSION: u8 = 3;

#[event]
pub struct PoolInitializedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub n_bands: u8,
    pub center_price_1e6: u64,
    pub width_bps: u16,
}

#[event]
pub struct PoolMigratedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub from: u8,
    pub to: u8,
    pub migrated_at_slot: u64,
}


#[event]
pub struct BandsDigestUpdatedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub width_bps: u16,
    pub center_price_1e6: u64,
    pub total_weight_bps: u32,
    pub hash: [u8; 32],
    pub slot: u64,
    pub fee_current_bps: u16,
    pub vol_ema_bps: u16,
}

#[event]
pub struct SimulatedBandsDigestV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub width_bps: u16,
    pub center_price_1e6: u64,
    pub hash: [u8; 32],
}

#[event]
pub struct LiquidityAddedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub band_idx: u8,
    pub shares: u64,
    pub receipt: Pubkey,
}

#[event]
pub struct LiquidityRemovedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub band_idx: u8,
    pub shares_burned: u64,
    pub out_a: u64,
    pub out_b: u64,
    pub receipt: Pubkey,
}

#[event]
pub struct FeesCollectedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub band_idx: u8,
    pub out_a: u64,
    pub out_b: u64,
}

#[event]
pub struct OrderPlacedV3 {
    pub event_version: u8,
    pub pool: Pubkey,
    pub order_id: u64,
    pub owner: Pubkey,
    pub side: Side,
    pub price_1e6: u64,
    pub qty: u64,
    pub band_idx: i16,
}

#[event]
pub struct OrderFilledV3 {
    pub event_version: u8,
    pub pool: Pubkey,
    pub taker: Pubkey,
    pub side: Side,
    pub qty: u64,
    pub price_1e6: u64,
    pub taker_fee_bps: u16,
    pub maker_rebate_bps: u16,
}

#[event]
pub struct OrderCanceledV3 {
    pub event_version: u8,
    pub pool: Pubkey,
    pub order_id: u64,
    pub owner: Pubkey,
    pub side: Side,
}

#[event]
pub struct SwapFilledV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub side: Side,
    pub qty: u64,
    pub price_1e6: u64,
    pub band_idx: u16,
    pub fee_bps: u16,
}

#[event]
pub struct DepthSnapshotV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub items: Vec<DepthItem>,
}

#[event]
pub struct ParamsProposedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub queued_at: u64,
}

#[event]
pub struct ParamsExecutedV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub executed_at: u64,
}

#[event]
pub struct EmergencyDrainV {
    pub event_version: u8,
    pub pool: Pubkey,
    pub amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct DepthItem {
    pub price_1e6: u64,
    pub bid_qty: u64,
    pub ask_qty: u64,
    pub band_idx: u16,
}

/* ------------------------------ Errors ------------------------------------ */

#[error_code]
pub enum DlmmError {
    #[msg("Bad multisig")]
    BadMultisig,
    #[msg("Invalid n bands")]
    InvalidNBands,
    #[msg("Proposal already exists")]
    ProposalExists,
    #[msg("Already migrated")]
    AlreadyMigrated,
    #[msg("No pending params")]
    NoPendingParams,
    #[msg("Proposal already executed")]
    ProposalExecuted,
    #[msg("Window closed")]
    WindowClosed,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Missing oracle signer")]
    MissingOracleSigner,
    #[msg("CU price too low")]
    CuPriceTooLow,
    #[msg("Cooldown not elapsed")]
    CooldownNotElapsed,
    #[msg("Deviation too high")]
    DeviationTooHigh,
    #[msg("Hysteresis not met")]
    HysteresisNotMet,
    #[msg("Vault mint mismatch")]
    VaultMintMismatch,
    #[msg("Invariant violated")]
    InvariantViolated,
    #[msg("Non monotonic bands")]
    NonMonotonicBands,
    #[msg("Invalid band range")]
    InvalidBandRange,
    #[msg("Weight sum invalid")]
    WeightSumInvalid,
    #[msg("Parameter out of range")]
    ParamOutOfRange,
    #[msg("Deposit ratio out of bounds")]
    DepositRatioOutOfBounds,
    #[msg("Band inactive")]
    BandInactive,
    #[msg("Zero shares")]
    ZeroShares,
    #[msg("Position locked")]
    PositionLocked,
    #[msg("Zero amount")]
    ZeroAmount,
    #[msg("Invalid band index")]
    InvalidBandIndex,
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Paused")]
    Paused,
    #[msg("Not paused")]
    NotPaused,
    #[msg("Not found")]
    NotFound,
    #[msg("Bad multisig quorum")]
    BadQuorum,
}

/* --------------------------- Small helpers -------------------------------- */

fn is_admin(admins: &[Pubkey; MAX_ADMINS], candidate: &Pubkey) -> bool {
    admins.iter().any(|a| a == candidate)
}
fn count_nonzero_admins(admins: &[Pubkey; MAX_ADMINS]) -> usize {
    admins.iter().filter(|p| **p != Pubkey::default()).count()
}
fn is_quorum(admins: &[Pubkey; MAX_ADMINS], threshold: u8, remaining: &[AccountInfo]) -> Result<bool> {
    let mut found = 0usize;
    for ra in remaining.iter() {
        if !ra.is_signer { continue; }
        let k = ra.key;
        for a in admins.iter() {
            if a == k { found += 1; break; }
        }
    }
    Ok(found >= (threshold as usize))
}

fn apply_settable_params(pool: &mut Pool, s: &SettableParamsV3) -> Result<()> {
    if let Some(v) = s.n_bands { require!(v as usize <= MAX_BANDS, DlmmError::InvalidNBands); pool.n_bands = v; }
    if let Some(v) = s.base_width_bps { pool.base_width_bps = v; }
    if let Some(v) = s.min_width_bps { pool.min_width_bps = v; }
    if let Some(v) = s.max_width_bps { pool.max_width_bps = v; }
    if let Some(v) = s.width_slope_per_kbps { pool.width_slope_per_kbps = v; }
    if let Some(v) = s.bias_per_kbps { pool.bias_per_kbps = v; }
    if let Some(v) = s.decay_per_band_bps { pool.decay_per_band_bps = v; }
    if let Some(v) = s.alpha_y_bps { pool.alpha_y_bps = v; }
    if let Some(v) = s.alpha_spot_bps { pool.alpha_spot_bps = v; }
    if let Some(v) = s.alpha_twap_bps { pool.alpha_twap_bps = v; }
    if let Some(v) = s.alpha_vol_bps { pool.alpha_vol_bps = v; }
    if let Some(v) = s.max_twap_dev_bps { pool.max_twap_dev_bps = v; }
    if let Some(v) = s.fee_base_bps { pool.fee_base_bps = v; }
    if let Some(v) = s.fee_k_per_bps { pool.fee_k_per_bps = v; }
    if let Some(v) = s.fee_max_bps { pool.fee_max_bps = v; }
    if let Some(v) = s.hyst_center_bps { pool.hyst_center_bps = v; }
    if let Some(v) = s.hyst_width_bps { pool.hyst_width_bps = v; }
    if let Some(v) = s.hyst_required_n { pool.hyst_required_n = v; }
    if let Some(v) = s.deposit_ratio_min_bps { pool.deposit_ratio_min_bps = v; }
    if let Some(v) = s.deposit_ratio_max_bps { pool.deposit_ratio_max_bps = v; }
    if let Some(v) = s.inactive_floor_a { pool.inactive_floor_a = v; }
    if let Some(v) = s.inactive_floor_b { pool.inactive_floor_b = v; }
    if let Some(v) = s.bounty_rate_microunits { pool.bounty_rate_microunits = v; }
    if let Some(v) = s.bounty_max { pool.bounty_max = v; }
    if let Some(v) = s.stale_slots_for_boost { pool.stale_slots_for_boost = v; }
    if let Some(v) = s.bounty_boost_bps { pool.bounty_boost_bps = v; }
    if let Some(v) = s.min_cu_price { pool.min_cu_price = v; }
    if let Some(v) = s.max_center_move_bps { pool.max_center_move_bps = v; }
    if let Some(v) = s.max_width_change_bps { pool.max_width_change_bps = v; }
    if let Some(v) = s.max_weight_shift_bps { pool.max_weight_shift_bps = v; }
    if let Some(v) = s.min_update_interval_slots { pool.min_update_interval_slots = v; }
    if let Some(v) = s.maker_rebate_max_bps { pool.maker_rebate_max_bps = v; }
    if let Some(v) = s.taker_min_bps { pool.taker_min_bps = v; }
    if let Some(v) = s.stp_mode { pool.stp_mode = v as u8; }
    if let Some(v) = s.route_mode { pool.route_mode = v as u8; }
    Ok(())
}

fn quote_shares_to_mint(b: &Band, add_a: u64, add_b: u64) -> u64 {
    add_a.max(add_b)
}

/* ================================ Constants ================================= */

pub const MAX_ADMINS: usize = 8;
pub const MAX_BANDS: usize = 64;
pub const METRICS_CAP: usize = 128;
pub const EVENT_Q_CAP: usize = 256;
pub const DEFAULT_MAX_QUEUE_PER_LEVEL: u16 = 64;

pub const POOL_SPACE: usize = 16 * 1024;
pub const ORDERBOOK_SPACE: usize = 16 * 1024;
pub const METRICS_SPACE: usize = 8_000;

pub const OUT_REASON_CANCEL: u8 = 1;

/* =============================================================================
                                   End
============================================================================= */
