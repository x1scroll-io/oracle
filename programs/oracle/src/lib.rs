use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("J3hMjTHy3M7pL8sf7X1Ey2cvaDQnefDTjzpoNoTXYaL9"); // replace after deploy

// ── CONSTANTS (immutable) ─────────────────────────────────────────────────────
const TREASURY: &str = "A1TRS3i2g62Zf6K4vybsW4JLx8wifqSoThyTQqXNaLDK";
const BURN_ADDRESS: &str = "1nc1nerator11111111111111111111111111111111";

const TREASURY_BPS: u64 = 5000;   // 50% treasury
const BURN_BPS: u64 = 5000;       // 50% burned 🔥
const BASIS_POINTS: u64 = 10000;

// Minimum attester bond: 1000 XNT (skin in the game)
const MIN_ATTESTER_BOND: u64 = 1_000_000_000_000; // 1000 XNT

// Feed request fee: 0.01 XNT per query
const FEED_FEE: u64 = 10_000_000; // 0.01 XNT

// Slash: 10% of bond for false attestation (majority vote decides)
const SLASH_BPS: u64 = 1000;

// Attestation threshold: 2/3 majority required
const QUORUM_BPS: u64 = 6667; // 66.67%

// Max attesters in oracle network
const MAX_ATTESTERS: usize = 50;

// Max feed types
const MAX_FEEDS: usize = 20;

// Feed staleness: reject if older than 5 slots
const MAX_STALENESS_SLOTS: u64 = 5;

// Unbond cooldown: 7 epochs
const UNBOND_EPOCHS: u64 = 7;

/// Feed types the oracle supports
/// Each feed type has different attestation logic
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum FeedType {
    ValidatorApy,       // APY delivered by a validator last epoch
    XntPrice,           // XNT/USD price
    RandomSeed,         // Verifiable random seed for games/lotteries
    ComplianceCheck,    // Wallet not on sanctions list (ZK-lite)
    ValidatorUptime,    // Validator uptime % over N epochs
    Custom,             // Custom data feed
}

#[program]
pub mod oracle {
    use super::*;

    /// Initialize oracle registry (once)
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.authority = ctx.accounts.authority.key();
        state.attester_count = 0;
        state.feed_count = 0;
        state.total_fees_collected = 0;
        state.total_burned = 0;
        state.total_attestations = 0;
        state.bump = ctx.bumps.state;
        Ok(())
    }

    /// Register as an oracle attester
    /// Bond 1000+ XNT — slashed for false attestations
    pub fn register_attester(ctx: Context<RegisterAttester>, bond_amount: u64) -> Result<()> {
        let state = &mut ctx.accounts.state;
        require!((state.attester_count as usize) < MAX_ATTESTERS, OracleError::MaxAttesters);
        require!(bond_amount >= MIN_ATTESTER_BOND, OracleError::BondTooSmall);

        let identity = ctx.accounts.attester.key();
        for i in 0..state.attester_count as usize {
            require!(state.attesters[i].identity != identity, OracleError::AlreadyRegistered);
        }

        // Lock bond
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.attester.to_account_info(),
                to: ctx.accounts.bond_vault.to_account_info(),
            }), bond_amount)?;

        let idx = state.attester_count as usize;
        state.attesters[idx] = AttesterEntry {
            identity,
            bond_amount,
            attestations_submitted: 0,
            attestations_correct: 0,
            slash_count: 0,
            active: true,
            unbonding: false,
            unbond_epoch: 0,
            joined_epoch: Clock::get()?.epoch,
        };
        state.attester_count += 1;

        emit!(AttesterRegistered { identity, bond: bond_amount, epoch: Clock::get()?.epoch });
        Ok(())
    }

    /// Register a new feed type
    /// Authority-only in phase 1, opens to anyone in phase 2
    pub fn register_feed(
        ctx: Context<RegisterFeed>,
        feed_type: FeedType,
        name: [u8; 32],
        description: [u8; 64],
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        // Phase 1: authority only
        require!(ctx.accounts.caller.key() == state.authority, OracleError::Unauthorized);
        require!((state.feed_count as usize) < MAX_FEEDS, OracleError::MaxFeeds);

        let idx = state.feed_count as usize;
        state.feeds[idx] = FeedEntry {
            feed_type,
            name,
            description,
            latest_value: 0,
            latest_slot: 0,
            latest_epoch: 0,
            attestation_count: 0,
            active: true,
        };
        state.feed_count += 1;

        emit!(FeedRegistered { feed_type_id: idx as u32, slot: Clock::get()?.slot });
        Ok(())
    }

    /// Submit an attestation for a feed
    /// Multiple attesters submit — value accepted when quorum reached
    pub fn submit_attestation(
        ctx: Context<SubmitAttestation>,
        feed_index: u32,
        value: u64,           // the attested value (APY bps, price * 1e6, etc.)
        confidence: u16,       // attester's confidence 0-10000 bps
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.attester.key();
        let current_slot = Clock::get()?.slot;
        let current_epoch = Clock::get()?.epoch;

        // Verify attester is registered and active
        let mut attester_idx = None;
        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == identity && state.attesters[i].active {
                attester_idx = Some(i);
                break;
            }
        }
        require!(attester_idx.is_some(), OracleError::NotAnAttester);

        let fidx = feed_index as usize;
        require!(fidx < state.feed_count as usize, OracleError::FeedNotFound);
        require!(state.feeds[fidx].active, OracleError::FeedInactive);

        // Record attestation
        let attestation_slot = ctx.accounts.attestation.to_account_info();
        let att = &mut ctx.accounts.attestation;
        att.feed_index = feed_index;
        att.attester = identity;
        att.value = value;
        att.confidence = confidence;
        att.submitted_slot = current_slot;
        att.submitted_epoch = current_epoch;
        att.counted = false;
        att.bump = ctx.bumps.attestation;

        state.attesters[attester_idx.unwrap()].attestations_submitted += 1;
        state.total_attestations += 1;

        emit!(AttestationSubmitted {
            feed_index,
            attester: identity,
            value,
            slot: current_slot,
        });

        Ok(())
    }

    /// Finalize a feed value — compute weighted median from recent attestations
    /// Anyone can call — permissionless aggregation
    pub fn finalize_feed(
        ctx: Context<FinalizeFeed>,
        feed_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let current_slot = Clock::get()?.slot;

        let fidx = feed_index as usize;
        require!(fidx < state.feed_count as usize, OracleError::FeedNotFound);

        // Simple aggregation: use the submitted value directly (phase 1)
        // Phase 2: collect multiple attestations, compute weighted median
        let value = ctx.accounts.latest_attestation.value;
        let attester = ctx.accounts.latest_attestation.attester;

        // Verify attestation is fresh
        require!(
            current_slot - ctx.accounts.latest_attestation.submitted_slot <= MAX_STALENESS_SLOTS,
            OracleError::StaleFeed
        );

        // Update feed
        state.feeds[fidx].latest_value = value;
        state.feeds[fidx].latest_slot = current_slot;
        state.feeds[fidx].latest_epoch = Clock::get()?.epoch;
        state.feeds[fidx].attestation_count += 1;

        // Collect feed fee — 50% treasury / 50% burned
        let fee = FEED_FEE;
        let treasury_fee = fee * TREASURY_BPS / BASIS_POINTS;
        let burn_fee = fee - treasury_fee;

        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.fee_payer.to_account_info(),
                to: ctx.accounts.treasury.to_account_info(),
            }), treasury_fee)?;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.fee_payer.to_account_info(),
                to: ctx.accounts.burn_address.to_account_info(),
            }), burn_fee)?;

        state.total_fees_collected = state.total_fees_collected.checked_add(fee).ok_or(OracleError::MathOverflow)?;
        state.total_burned = state.total_burned.checked_add(burn_fee).ok_or(OracleError::MathOverflow)?;

        emit!(FeedFinalized { feed_index, value, attester, slot: current_slot });
        Ok(())
    }

    /// Read a feed value (free — anyone can read on-chain state)
    /// Off-chain clients read directly from state account
    /// On-chain programs read via CPI and check freshness

    /// Slash attester for false attestation
    /// Requires majority of other attesters to agree (quorum)
    pub fn slash_attester(
        ctx: Context<SlashAttester>,
        attester_identity: Pubkey,
        feed_index: u32,
        evidence_slot: u64,    // slot of the disputed attestation
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        // Phase 1: authority-controlled slashing
        require!(ctx.accounts.authority.key() == state.authority, OracleError::Unauthorized);

        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == attester_identity {
                let slash_amount = state.attesters[i].bond_amount * SLASH_BPS / BASIS_POINTS;
                let treasury_cut = slash_amount * TREASURY_BPS / BASIS_POINTS;
                let burn_cut = slash_amount - treasury_cut;

                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer {
                        from: ctx.accounts.bond_vault.to_account_info(),
                        to: ctx.accounts.treasury.to_account_info(),
                    }), treasury_cut)?;
                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer {
                        from: ctx.accounts.bond_vault.to_account_info(),
                        to: ctx.accounts.burn_address.to_account_info(),
                    }), burn_cut)?;

                state.attesters[i].bond_amount = state.attesters[i].bond_amount
                    .checked_sub(slash_amount).ok_or(OracleError::MathOverflow)?;
                state.attesters[i].slash_count += 1;
                state.total_burned = state.total_burned.checked_add(burn_cut).ok_or(OracleError::MathOverflow)?;

                emit!(AttesterSlashed { identity: attester_identity, slash_amount, burned: burn_cut, epoch: Clock::get()?.epoch });
                return Ok(());
            }
        }
        Err(OracleError::AttesterNotFound.into())
    }

    /// Begin unbonding (7 epoch cooldown)
    pub fn begin_unbond(ctx: Context<BeginUnbond>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.attester.key();
        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == identity {
                require!(!state.attesters[i].unbonding, OracleError::AlreadyUnbonding);
                state.attesters[i].unbonding = true;
                state.attesters[i].unbond_epoch = Clock::get()?.epoch + UNBOND_EPOCHS;
                state.attesters[i].active = false; // stop receiving attestation duties
                emit!(UnbondStarted { identity, release_epoch: state.attesters[i].unbond_epoch });
                return Ok(());
            }
        }
        Err(OracleError::AttesterNotFound.into())
    }

    /// Complete withdrawal after unbond period
    pub fn complete_withdraw(ctx: Context<CompleteWithdraw>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.attester.key();
        let current_epoch = Clock::get()?.epoch;

        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == identity {
                require!(state.attesters[i].unbonding, OracleError::NotUnbonding);
                require!(current_epoch >= state.attesters[i].unbond_epoch, OracleError::UnbondNotReady);

                let amount = state.attesters[i].bond_amount;
                require!(amount > 0, OracleError::NothingToClaim);

                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer {
                        from: ctx.accounts.bond_vault.to_account_info(),
                        to: ctx.accounts.attester.to_account_info(),
                    }), amount)?;

                state.attesters[i].bond_amount = 0;
                state.attesters[i].unbonding = false;

                emit!(BondWithdrawn { identity, amount, epoch: current_epoch });
                return Ok(());
            }
        }
        Err(OracleError::AttesterNotFound.into())
    }
}

// ── ACCOUNTS ──────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = 8 + OracleState::LEN, seeds = [b"oracle"], bump)]
    pub state: Account<'info, OracleState>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterAttester<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    #[account(mut)]
    pub attester: Signer<'info>,
    /// CHECK: bond vault
    #[account(mut, seeds = [b"oracle-vault"], bump)]
    pub bond_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterFeed<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub caller: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(feed_index: u32, value: u64, confidence: u16)]
pub struct SubmitAttestation<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    #[account(
        init,
        payer = attester,
        space = 8 + AttestationRecord::LEN,
        seeds = [b"attestation", feed_index.to_le_bytes().as_ref(), attester.key().as_ref()],
        bump,
    )]
    pub attestation: Account<'info, AttestationRecord>,
    #[account(mut)]
    pub attester: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeFeed<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub latest_attestation: Account<'info, AttestationRecord>,
    #[account(mut)]
    pub fee_payer: Signer<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ OracleError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ OracleError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SlashAttester<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub authority: Signer<'info>,
    /// CHECK: vault
    #[account(mut, seeds = [b"oracle-vault"], bump)]
    pub bond_vault: AccountInfo<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ OracleError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ OracleError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BeginUnbond<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub attester: Signer<'info>,
}

#[derive(Accounts)]
pub struct CompleteWithdraw<'info> {
    #[account(mut, seeds = [b"oracle"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    #[account(mut)]
    pub attester: Signer<'info>,
    /// CHECK: vault returns bond
    #[account(mut, seeds = [b"oracle-vault"], bump)]
    pub bond_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

// ── STATE ─────────────────────────────────────────────────────────────────────

#[account]
pub struct OracleState {
    pub authority: Pubkey,
    pub attester_count: u32,
    pub feed_count: u32,
    pub total_fees_collected: u64,
    pub total_burned: u64,
    pub total_attestations: u64,
    pub bump: u8,
    pub attesters: [AttesterEntry; 50],
    pub feeds: [FeedEntry; 20],
}

impl OracleState {
    pub const LEN: usize = 32 + 4 + 4 + 8 + 8 + 8 + 1
        + (AttesterEntry::LEN * 50)
        + (FeedEntry::LEN * 20);
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct AttesterEntry {
    pub identity: Pubkey,
    pub bond_amount: u64,
    pub attestations_submitted: u64,
    pub attestations_correct: u64,
    pub slash_count: u32,
    pub active: bool,
    pub unbonding: bool,
    pub unbond_epoch: u64,
    pub joined_epoch: u64,
}
impl AttesterEntry { pub const LEN: usize = 32 + 8 + 8 + 8 + 4 + 1 + 1 + 8 + 8; }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct FeedEntry {
    pub feed_type: FeedType,
    pub name: [u8; 32],
    pub description: [u8; 64],
    pub latest_value: u64,
    pub latest_slot: u64,
    pub latest_epoch: u64,
    pub attestation_count: u64,
    pub active: bool,
}
impl FeedEntry { pub const LEN: usize = 1 + 32 + 64 + 8 + 8 + 8 + 8 + 1; }

#[account]
pub struct AttestationRecord {
    pub feed_index: u32,
    pub attester: Pubkey,
    pub value: u64,
    pub confidence: u16,
    pub submitted_slot: u64,
    pub submitted_epoch: u64,
    pub counted: bool,
    pub bump: u8,
}
impl AttestationRecord { pub const LEN: usize = 4 + 32 + 8 + 2 + 8 + 8 + 1 + 1; }

// ── EVENTS ────────────────────────────────────────────────────────────────────

#[event]
pub struct AttesterRegistered { pub identity: Pubkey, pub bond: u64, pub epoch: u64 }
#[event]
pub struct FeedRegistered { pub feed_type_id: u32, pub slot: u64 }
#[event]
pub struct AttestationSubmitted { pub feed_index: u32, pub attester: Pubkey, pub value: u64, pub slot: u64 }
#[event]
pub struct FeedFinalized { pub feed_index: u32, pub value: u64, pub attester: Pubkey, pub slot: u64 }
#[event]
pub struct AttesterSlashed { pub identity: Pubkey, pub slash_amount: u64, pub burned: u64, pub epoch: u64 }
#[event]
pub struct UnbondStarted { pub identity: Pubkey, pub release_epoch: u64 }
#[event]
pub struct BondWithdrawn { pub identity: Pubkey, pub amount: u64, pub epoch: u64 }

// ── ERRORS ────────────────────────────────────────────────────────────────────

#[error_code]
pub enum OracleError {
    #[msg("Maximum attesters reached (50)")]
    MaxAttesters,
    #[msg("Maximum feeds reached (20)")]
    MaxFeeds,
    #[msg("Bond below minimum (1000 XNT)")]
    BondTooSmall,
    #[msg("Already registered as attester")]
    AlreadyRegistered,
    #[msg("Not a registered attester")]
    NotAnAttester,
    #[msg("Attester not found")]
    AttesterNotFound,
    #[msg("Feed not found")]
    FeedNotFound,
    #[msg("Feed is inactive")]
    FeedInactive,
    #[msg("Feed data is stale — resubmit")]
    StaleFeed,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Already unbonding")]
    AlreadyUnbonding,
    #[msg("Not in unbonding")]
    NotUnbonding,
    #[msg("Unbond period not complete")]
    UnbondNotReady,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid treasury address")]
    InvalidTreasury,
    #[msg("Invalid burn address")]
    InvalidBurnAddress,
}
